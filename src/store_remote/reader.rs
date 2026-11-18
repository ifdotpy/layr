//! Read, authenticate and restore immutable records and state attempts.

use super::{
    backend::{Backend, Undecodable},
    format::{self, Entry, Header, Tree},
    keys, zst, Options, Report, Store,
};
use crate::{ctx::Ctx, model::Record, oplog, repo::Repo};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_hashers::{ContentAddress, Digest};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[path = "fetch_plan.rs"]
mod fetch_plan;
use fetch_plan::{unchanged, Material};

use crate::sync_local::fetch::publish_restored;
#[cfg(test)]
use crate::sync_local::fetch::walk_ancestry;

#[derive(Debug)]
pub(super) struct Records {
    pub lines: Vec<String>,
    pub keys: BTreeMap<String, VerifyingKey>,
    pub overlaps: usize,
    pub gaps: Vec<String>,
}

pub(super) fn records(backend: &Backend, identity: &str, project: &str) -> Result<Records> {
    records_with_local(backend, identity, project, &[], None)
}

/// Checked segments (`key modified digest`): `seen-records` in the store's local folder.
pub(super) fn seen_file(dir: &Path) -> PathBuf {
    dir.join("seen-records")
}

/// Reuse a checked segment only if its modification time and the digest of its complete local range match.
/// Old stamps without a digest require a read. Fetch imports the checked lines after this returns; hydration
/// does not, so sequence coverage alone cannot establish that the local records match a checked segment.
pub(super) fn records_with_local(
    backend: &Backend,
    identity: &str,
    project: &str,
    local: &[String],
    seen: Option<&Path>,
) -> Result<Records> {
    let mut machines: BTreeMap<String, BTreeMap<u64, String>> = BTreeMap::new();
    let mut overlaps = 0;
    let known: std::collections::BTreeSet<String> = seen
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.lines().map(str::to_owned).collect())
        .unwrap_or_default();
    let mut have: BTreeMap<String, BTreeMap<u64, &str>> = BTreeMap::new();
    if !known.is_empty() {
        for line in local {
            let record = oplog::parse_line(line)?.record;
            have.entry(record.machine).or_default().insert(record.seq, line);
        }
    }
    let mut checked = Vec::new();
    let mut wanted = Vec::new();
    for object in backend.list("records/")? {
        let (machine, first, last) = format::parse_segment_name(&object.key).context("invalid record segment name")?;
        let stamp = format!("{} {}", object.key, object.modified);
        let local_digest = || -> Option<String> {
            let by_seq = have.get(&machine)?;
            let mut digest = ContentAddress::new();
            for seq in first..=last {
                digest.update(by_seq.get(&seq)?.as_bytes());
                digest.update(b"\n");
            }
            Some(hex::encode(digest.finalize()))
        };
        if let Some(bound) = local_digest().map(|digest| format!("{stamp} {digest}")).filter(|bound| known.contains(bound)) {
            checked.push(bound);
        } else {
            wanted.push((object.key, machine, first, last, stamp, object.size));
        }
    }
    // A segment is one small object: a clone reads hundreds of them, so they come in batches (one request per
    // 256 segments through a gateway, else several reads at a time). Each is checked as it comes, while its read holds
    // its reservation: the bodies of a long history are never all in memory.
    let names: Vec<String> = wanted.iter().map(|(key, ..)| key.clone()).collect();
    let sizes: Vec<Option<u64>> = wanted.iter().map(|(.., size)| *size).collect();
    backend.visit_many(&names, &sizes, |i, body| {
        let (key, machine, first, last, stamp, _) = &wanted[i];
        let (first, last) = (*first, *last);
        let body = String::from_utf8(keys::decrypt(identity, &body.context("record segment disappeared")?)?)?;
        let mut expected = first;
        let mut digest = ContentAddress::new();
        for line in body.lines() {
            let signed = oplog::parse_line(line)?;
            if &signed.record.machine != machine || signed.record.seq != expected || expected > last {
                bail!("segment {} has an invalid sequence", key);
            }
            if signed.record.project != project {
                bail!("segment {} belongs to another project", key);
            }
            crate::model::validate(&signed.record)
                .with_context(|| format!("record {} of machine {machine} ({})", signed.record.seq, signed.record.id))?;
            let by_seq = machines.entry(machine.clone()).or_default();
            if let Some(previous) = by_seq.get(&expected) {
                if previous != line {
                    bail!("conflicting history of machine {machine}, record {expected}");
                }
                overlaps += 1;
            } else {
                by_seq.insert(expected, line.to_string());
            }
            digest.update(line.as_bytes());
            digest.update(b"\n");
            expected = expected.checked_add(1).context("record sequence overflow")?;
        }
        if expected.checked_sub(1) != Some(last) {
            bail!("segment {} is incomplete", key);
        }
        checked.push(format!("{stamp} {}", hex::encode(digest.finalize())));
        Ok(())
    })?;
    // A fetch can continue from the reader's verified prefix when old remote segments are absent.
    for line in local {
        let record = oplog::parse_line(line)?.record;
        if record.project != project {
            bail!("local records belong to another project");
        }
        let by_seq = machines.entry(record.machine.clone()).or_default();
        if let Some(existing) = by_seq.get(&record.seq).filter(|existing| *existing != line) {
            if record.seq == 1 {
                let remote = oplog::parse_line(existing)?.record;
                if oplog::machine_key(&remote)? != oplog::machine_key(&record)? {
                    bail!("remote machine {}: first-record key does not match the known public key", record.machine);
                }
            }
            bail!("conflicting history of machine {}, record {}", record.machine, record.seq);
        }
        by_seq.entry(record.seq).or_insert_with(|| line.clone());
    }
    let mut keys = BTreeMap::new();
    let mut lines = Vec::new();
    let mut gaps = Vec::new();
    for (machine, by_seq) in machines {
        let Some(first) = by_seq.get(&1) else {
            gaps.push(format!("machine {machine}: missing record 1"));
            continue;
        };
        let first = oplog::parse_line(first)?;
        if first.record.op != "machine" {
            bail!("machine {machine}: first record is not its key");
        }
        let key = oplog::machine_key(&first.record)?;
        // Verify the complete segment set, including records beyond a gap.
        for line in by_seq.values() {
            oplog::verify_line(line, &key)?;
        }
        let mut seq = 1;
        while let Some(line) = by_seq.get(&seq) {
            lines.push(line.clone());
            seq += 1;
        }
        if by_seq.keys().next_back().is_some_and(|last| *last >= seq) {
            gaps.push(format!("machine {machine}: missing record {seq}"));
        }
        keys.insert(machine, key);
    }
    lines.sort_by_key(|l| {
        let r = oplog::parse_line(l).expect("record checked").record;
        (r.time, r.machine, r.seq)
    });
    if let Some(path) = seen {
        // Only after every segment was checked: a failed read leaves the old file. The first fetch of an overlay
        // layer makes its folder.
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(anyhow::Error::from)
            .and_then(|_| crate::fsutil::atomic_write(path, format!("{}\n", checked.join("\n")).as_bytes(), 0o600));
        if let Err(e) = written {
            eprintln!("warning: {}: {e:#}", path.display());
        }
    }
    Ok(Records { lines, keys, overlaps, gaps })
}

/// The most parts of one prefetch window: one batch of the gateway.
const WINDOW_KEYS: usize = 256;

/// The parts of one prefetch window of full parts, the window when no listing gave the sizes of the parts: they stay
/// in the operation cache until restore reads them, so a window takes at most half of it.
#[cfg(test)]
pub(super) fn window_parts() -> usize {
    usize::try_from(fetch_plan::MATERIAL_BYTES / 2 / keys::ciphertext_len(format::PART_LIMIT) as u64).unwrap_or(1).max(1)
}

pub(super) struct Reader<'a> {
    backend: &'a Backend,
    identity: &'a str,
    keys: &'a BTreeMap<String, VerifyingKey>,
    trees: BTreeMap<(String, String), (Header, Vec<u8>, Tree)>,
    depths: BTreeMap<(String, String), usize>,
    parts: BTreeMap<String, Vec<u8>>,
    part_bytes: usize,
    gets: std::cell::Cell<usize>,
    chunks: Option<super::chunked::Reader<'a>>,
    material: Option<Material>,
    part_order: Vec<String>,
    // Object -> (plan position, remaining uses, large signed frame extent).
    part_uses: BTreeMap<String, (usize, usize, bool)>,
    /// Listed sizes of remote objects (`backend::Object::size`): they reserve the bytes of their reads.
    pub(super) sizes: BTreeMap<String, u64>,
    chunk_dir: Option<PathBuf>,
}

impl<'a> Reader<'a> {
    pub fn new(backend: &'a Backend, identity: &'a str, keys: &'a BTreeMap<String, VerifyingKey>) -> Self {
        Self {
            backend,
            identity,
            keys,
            trees: BTreeMap::new(),
            depths: BTreeMap::new(),
            parts: BTreeMap::new(),
            part_bytes: 0,
            gets: std::cell::Cell::new(0),
            chunks: None,
            material: None,
            part_order: Vec::new(),
            part_uses: BTreeMap::new(),
            sizes: BTreeMap::new(),
            chunk_dir: None,
        }
    }
    pub fn get_count(&self) -> usize {
        self.gets.get()
    }
    /// Keep downloaded objects of this operation in an encrypted cache in `dir`, so that batched prefetches and
    /// evicted trees are not read from the remote again. Without a usable folder, every read goes to the remote.
    pub(super) fn cache_in(&mut self, dir: Option<PathBuf>) {
        self.material = dir.and_then(Material::new);
    }
    fn decrypted(&mut self, key: &str) -> Result<Vec<u8>> {
        if let Some(body) = self.material.as_mut().and_then(|m| m.get(key)) {
            return Ok(body);
        }
        if let (Some(material), Some(&(position, _, large))) = (&mut self.material, self.part_uses.get(key)) {
            // No retained part buffers overlap WP1's transport/decrypt workspace.
            self.parts.clear();
            self.part_bytes = 0;
            // A window holds at most half of the operation cache: a part of unknown size counts as a full one.
            let full = keys::ciphertext_len(format::PART_LIMIT) as u64;
            let mut bytes = 0;
            let names = self.part_order[position..]
                .iter()
                .take(if large { 1 } else { WINDOW_KEYS })
                .take_while(|name| large || !self.part_uses[*name].2)
                .take_while(|name| {
                    bytes += self.sizes.get(*name).copied().unwrap_or(full);
                    large || bytes <= fetch_plan::MATERIAL_BYTES / 2
                })
                .filter(|name| self.part_uses[*name].1 > 0 && !material.entries.contains_key(*name))
                .cloned()
                .collect::<Vec<_>>();
            let mut wanted = None;
            Self::download(self.backend, self.identity, &self.gets, &self.sizes, &names, large, |i, body| {
                material.put(&names[i], &body);
                if names[i] == key {
                    wanted = Some(body);
                }
                Ok(())
            })?;
            if let Some(body) = wanted {
                return Ok(body);
            }
        }
        self.gets.set(self.gets.get() + 1);
        let body = self.backend.decrypted(key, self.identity).with_context(|| format!("decrypt {key}"))?;
        if let Some(material) = &mut self.material {
            material.put(key, &body);
        }
        Ok(body)
    }

    /// One dispatch point for prefetch, in batches of 256 objects: capped Gateway batches for parts; otherwise one
    /// run of the transfer pool per batch (a File or S3 body without a listed size runs alone). An oversized part
    /// comes in a window of its own (`decrypted`).
    /// Reads `names` in batches. The listed size of each, or else the transport's read limit for `large` parts,
    /// reserves the bytes of its read.
    fn download(
        backend: &Backend,
        identity: &str,
        gets: &std::cell::Cell<usize>,
        sizes: &BTreeMap<String, u64>,
        names: &[String],
        large: bool,
        mut visit: impl FnMut(usize, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        let limit = backend.read_limit().filter(|_| large);
        for (batch, window) in names.chunks(WINDOW_KEYS).enumerate() {
            let offset = batch * WINDOW_KEYS;
            gets.set(gets.get() + window.len());
            let sizes: Vec<_> = window.iter().map(|name| sizes.get(name).copied().or(limit.map(|limit| limit as u64))).collect();
            backend.visit_decrypted_many(window, &sizes, identity, |i, body| visit(offset + i, body))?;
        }
        Ok(())
    }

    /// Fetch encrypted tree material by dependency rounds, deduplicating attempts.
    /// Headers only route reads; `tree` authenticates every canonical base before use.
    pub(super) fn prefetch_trees(&mut self, roots: Vec<(String, String)>) -> Result<()> {
        if roots.is_empty() {
            return Ok(());
        }
        let Some(material) = &mut self.material else { return Ok(()) };
        let mut seen = BTreeSet::new();
        let mut pending = roots;
        for _ in 0..format::FULL_TREE_EVERY {
            pending.retain(|at| seen.insert(at.clone()));
            if pending.is_empty() {
                return Ok(());
            }
            let names: Vec<_> = pending.iter().map(|(s, a)| format::tree_name(s, a)).collect();
            let mut next = Vec::new();
            Self::download(self.backend, self.identity, &self.gets, &self.sizes, &names, false, |i, object| {
                let (header, _) = format::split_tree(&object)?;
                // Routing is speculative. `tree` checks format, identity, signature
                // and canonical content before using any tree. Only safe names route reads.
                if header.tree != "full" {
                    if let Some(base) = &header.base {
                        if format::parse_tree_name(&format::tree_name(&base.0, &base.1)).is_some() && !seen.contains(base) {
                            next.push(base.clone());
                        }
                    }
                }
                material.put(&names[i], &object);
                Ok(())
            })?;
            pending = next;
        }
        anyhow::ensure!(pending.iter().all(|at| seen.contains(at)), "tree chain exceeds 32 states");
        Ok(())
    }
    pub fn tree(&mut self, state: &str, attempt: &str) -> Result<(Header, Vec<u8>, Tree)> {
        let wanted = (state.to_string(), attempt.to_string());
        if let Some(tree) = self.trees.get(&wanted) {
            return Ok(tree.clone());
        }
        let _decode = crate::trace::span("store decode tree");
        let mut at = wanted.clone();
        let mut pending = Vec::new();
        let mut seen = BTreeSet::new();
        let mut canonical;
        let mut depth = 0;
        // Every failure but a failed read, a missing object and a signer without a key here is `Undecodable`: the tree
        // never decodes.
        loop {
            if !seen.insert(at.clone()) {
                bail!(Undecodable("tree chain has a loop".into()));
            }
            if seen.len() > format::FULL_TREE_EVERY {
                bail!(Undecodable("tree chain exceeds 32 states".into()));
            }
            if let Some((_, bytes, _)) = self.trees.get(&at) {
                canonical = bytes.clone();
                depth = self.depths[&at];
                break;
            }
            let object = self.decrypted(&format::tree_name(&at.0, &at.1))?;
            let (header, frame) = format::split_tree(&object).map_err(Undecodable::of)?;
            if header.v != 2 {
                bail!(Undecodable(format!("unsupported tree format {}", header.v)));
            }
            if (header.state.as_str(), header.attempt.as_str()) != (at.0.as_str(), at.1.as_str()) {
                bail!(Undecodable("tree identity differs from its object name".into()));
            }
            let full = header.tree == "full";
            let base = header.base.clone();
            pending.push((header, frame.to_vec()));
            if full {
                canonical = Vec::new();
                break;
            }
            at = base.ok_or_else(|| Undecodable("delta tree has no base".into()))?;
        }
        while let Some((header, frame)) = pending.pop() {
            depth = if header.tree == "full" { 0 } else { depth + 1 };
            // A frame that does not decode is `Undecodable` whether or not its signer has a key here.
            let next = (|| {
                anyhow::ensure!(depth < format::FULL_TREE_EVERY, "tree chain exceeds 32 states");
                zst::decompress(&frame, if header.tree == "full" { None } else { Some(&canonical) })
            })()
            .map_err(Undecodable::of)?;
            let key = self.keys.get(&header.machine).context("tree signer has no verified machine record")?;
            let tree = (|| -> Result<Tree> {
                header.verify(&next, key)?;
                Ok(Tree::parse(&next)?)
            })()
            .map_err(Undecodable::of)?;
            canonical = next;
            // At most two complete delta chains, rather than every tree in project history.
            if self.trees.len() >= format::FULL_TREE_EVERY * 2 {
                self.trees.clear();
                self.depths.clear();
            }
            self.depths.insert((header.state.clone(), header.attempt.clone()), depth);
            self.trees.insert((header.state.clone(), header.attempt.clone()), (header, canonical.clone(), tree));
        }
        self.trees.get(&wanted).cloned().context("tree was not decoded")
    }
    pub fn tree_depth(&self, state: &str, attempt: &str) -> Result<usize> {
        self.depths.get(&(state.to_string(), attempt.to_string())).copied().context("tree depth was not decoded")
    }
    /// The content of the file `path` of `tree`, from frames or from chunks.
    pub fn read_file(&mut self, tree: &Tree, path: &str) -> Result<Vec<u8>> {
        let entry = tree.files.get(path).context("file is not in its tree")?;
        match tree.chunks.get(path) {
            Some(ids) => self.read_chunked(entry, ids),
            None => self.read(entry),
        }
    }
    fn chunks(&mut self) -> &mut super::chunked::Reader<'a> {
        self.chunks.get_or_insert_with(|| {
            let mut chunks = super::chunked::Reader::new(self.backend, self.identity);
            chunks.dir = self.chunk_dir.clone();
            chunks
        })
    }
    /// A chunked file of a cache layer (`Tree::chunks`), checked against its tree entry.
    pub fn read_chunked(&mut self, entry: &Entry, ids: &[String]) -> Result<Vec<u8>> {
        checked(entry, self.chunks().file(ids, entry.1)?)
    }
    pub fn read(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        if entry.3.is_empty() {
            bail!("a chunked file needs its chunk ids (Reader::read_file)");
        }
        let mut data = Vec::new();
        for (index, link) in entry.3.iter().enumerate().rev() {
            let key = link.object();
            if !self.parts.contains_key(&key) {
                let bytes = self.decrypted(&key)?;
                if self.part_bytes + bytes.len() > 64 << 20 {
                    self.parts.clear();
                    self.part_bytes = 0;
                }
                self.part_bytes += bytes.len();
                self.parts.insert(key.clone(), bytes);
            }
            let part = self.parts.get(&key).context("part was not loaded")?;
            let start = usize::try_from(link.3)?;
            let end = usize::try_from(link.3.checked_add(link.4).context("frame range overflow")?)?;
            let frame = part.get(start..end).context("frame is outside its part")?;
            let limit = if index == 0 { usize::try_from(entry.1)? } else { zst::LIMIT };
            data = zst::decompress_with_limit(frame, if link.5 == "delta" { Some(&data) } else { None }, limit)?;
            if let Some((_, uses, _)) = self.part_uses.get_mut(&key) {
                *uses = uses.saturating_sub(1);
                if *uses == 0 {
                    if let Some(material) = &mut self.material {
                        material.remove(&key);
                    }
                }
            }
        }
        checked(entry, data)
    }
    fn restore_over(&mut self, tree: &Tree, base: Option<&Tree>, root: &std::path::Path) -> Result<usize> {
        let _restore = crate::trace::span("store restore files");
        crate::fsutil::refuse_case_collisions(root, tree.files.keys().map(|k| k.as_str()))?;
        let target = crate::tree::Tree::open(root)?;
        if let Some(base) = base {
            for path in base.files.keys() {
                if !tree.files.contains_key(path) && target.stat(path).is_some() {
                    target.remove(path)?;
                }
            }
        }
        // The chunk reader leaves `self` while the loop below writes the files: the packs of its plan load on a pool
        // of this scope during the whole loop.
        self.chunks();
        let mut chunks = self.chunks.take().context("chunk reader")?;
        let written = std::thread::scope(|scope| self.write_files(tree, base, root, &target, &mut chunks, scope));
        self.chunks = Some(chunks);
        written
    }

    fn write_files<'scope, 'env>(
        &mut self,
        tree: &Tree,
        base: Option<&Tree>,
        root: &std::path::Path,
        target: &crate::tree::Tree,
        chunks: &mut super::chunked::Reader<'a>,
        scope: &'scope std::thread::Scope<'scope, 'env>,
    ) -> Result<usize>
    where
        'a: 'scope,
    {
        let mut planned = None;
        if !tree.chunks.is_empty() {
            // The chunks of the files that the loop below writes, in its order, so their packs load ahead.
            let ids: Vec<&String> = tree
                .files
                .iter()
                .filter(|(path, entry)| !unchanged(tree, base, path, entry))
                .filter_map(|(path, _)| tree.chunks.get(path))
                .flatten()
                .collect();
            // A chunk that no pack holds fails the restore before any file is read: the indexes are small, the files
            // are not.
            chunks.require(&ids)?;
            planned = Some(chunks.plan(scope, ids)?);
        }
        let mut written = 0;
        for (path, entry) in &tree.files {
            if unchanged(tree, base, path, entry) {
                continue;
            }
            let data = match (tree.chunks.get(path), planned.as_mut()) {
                (Some(ids), Some(planned)) => checked(entry, planned.next_file(ids, entry.1)?)?,
                (Some(_), None) => bail!("a chunked file outside the plan"),
                (None, _) => self.read(entry)?,
            };
            target.mkdirs_for(path, None)?;
            if target.stat(path).is_some() {
                target.remove(path)?;
            }
            if entry.0 == "120000" {
                use std::os::unix::ffi::OsStrExt;
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), crate::fsutil::safe_join(root, path)?)?;
            } else {
                use std::io::Write;
                let mut file = target.create(path, if entry.0 == "100755" { 0o755 } else { 0o644 })?;
                file.write_all(&data)?;
                // The time of the source file: a build tool compares it (`Tree::times`).
                if let Some(ns) = tree.times.get(path) {
                    file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_nanos(*ns))?;
                }
            }
            written += 1;
        }
        Ok(written)
    }
}

/// `data`, when it has the size and SHA-256 of its tree entry.
fn checked(entry: &Entry, data: Vec<u8>) -> Result<Vec<u8>> {
    if data.len() as u64 != entry.1 || hex::encode(ContentAddress::digest(&data)) != entry.2 {
        bail!("file size or SHA-256 does not match its tree");
    }
    Ok(data)
}

/// The machines that a project's own signed history trusts: the creator (the signer of the `project` record), then
/// each machine that a member trusts with a `machine.trust` record naming the key it really has. A machine that a
/// server serves but nobody in this chain vouched for is not in it.
pub(super) fn project_trust_chain(lines: &[String], keys: &BTreeMap<String, VerifyingKey>) -> Result<BTreeSet<String>> {
    let records: Vec<Record> = lines.iter().map(|line| Ok(oplog::parse_line(line)?.record)).collect::<Result<_>>()?;
    let mut chain: BTreeSet<String> = records.iter().filter(|r| r.op == "project").map(|r| r.machine.clone()).take(1).collect();
    loop {
        let vouched: Vec<String> = records
            .iter()
            .filter(|r| r.op == "machine.trust" && chain.contains(&r.machine))
            .filter_map(|r| {
                let (machine, key) = (r.data["machine"].as_str()?, r.data["public_key"].as_str()?);
                keys.get(machine).is_some_and(|k| hex::encode(k.to_bytes()) == key).then(|| machine.to_string())
            })
            .filter(|machine| !chain.contains(machine))
            .collect();
        if vouched.is_empty() {
            return Ok(chain);
        }
        chain.extend(vouched);
    }
}

/// The writer keys that a Layr Cloud remote served before (`writer-keys.json` of the remote folder): a server
/// cannot give a machine another key later.
pub(super) fn pin_writer_keys(dir: &Path, keys: &BTreeMap<String, VerifyingKey>) -> Result<()> {
    let path = dir.join("writer-keys.json");
    let mut pinned: BTreeMap<String, String> = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid writer-keys.json")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(e.into()),
    };
    let before = pinned.len();
    for (machine, key) in keys {
        let key = hex::encode(key.to_bytes());
        match pinned.get(machine) {
            Some(old) if old != &key => {
                bail!("Layr Cloud serves machine {machine} with another key than before; nothing is taken")
            }
            Some(_) => {}
            None => {
                pinned.insert(machine.clone(), key);
            }
        }
    }
    if pinned.len() != before {
        std::fs::create_dir_all(dir)?;
        crate::fsutil::atomic_write(&path, &serde_json::to_vec(&pinned)?, 0o600)?;
    }
    Ok(())
}

/// The states of the project for a read from the Layr Cloud API, which has no state listing: every state that
/// the records name. `fetch` asks the server which of them it holds, in one batch.
fn api_states(repo: &Repo) -> Result<BTreeMap<String, String>> {
    Ok(repo.p.db.state_auxiliary()?.into_iter().map(|(id, _)| (id, String::new())).collect())
}

/// The state trees of a store, for `restore_states`. A command that restores many states (migrate)
/// calls it once per state, so the list is kept for the process and read again only when a state
/// that it must restore is not in it.
fn restore_attempts(store: &Store, repo: &Repo, ids: &[String]) -> Result<BTreeMap<String, String>> {
    if ids.iter().all(|id| repo.p.has_state(id)) {
        return Ok(BTreeMap::new());
    }
    static KNOWN: std::sync::Mutex<BTreeMap<std::path::PathBuf, BTreeMap<String, String>>> =
        std::sync::Mutex::new(BTreeMap::new());
    let wanted = |states: &BTreeMap<String, String>| ids.iter().all(|id| repo.p.has_state(id) || states.contains_key(id));
    if let Some(states) = KNOWN.lock().ok().and_then(|known| known.get(&store.dir).filter(|s| wanted(s)).cloned()) {
        return Ok(states);
    }
    let states = attempts(&store.backend)?;
    if let Ok(mut known) = KNOWN.lock() {
        known.insert(store.dir.clone(), states.clone());
    }
    Ok(states)
}

pub(super) fn attempts(backend: &Backend) -> Result<BTreeMap<String, String>> {
    Ok(attempts_of(&backend.list("states/")?))
}

/// The first complete attempt of each state in a listing of `states/`.
fn attempts_of(objects: &[super::backend::Object]) -> BTreeMap<String, String> {
    let mut states = BTreeMap::new();
    for object in objects {
        if let Some((state, attempt)) = format::parse_tree_name(&object.key) {
            states.entry(state).or_insert(attempt);
        }
    }
    states
}

/// Write one missing state of the store into the project and return the files written. The
/// publish step takes the project lock unless this process holds it already.
fn restore_one(
    repo: &Repo,
    reader: &mut Reader<'_>,
    states: &BTreeMap<String, String>,
    state: &crate::model::StateRec,
) -> Result<usize> {
    let id = state.id.clone();
    let attempt = &states[&id];
    let (header, _, tree) = reader.tree(&id, &attempt)?;
    let stage = repo.p.work_dir().join(crate::store::temp_name());
    if header.btrfs.as_ref().is_some_and(|identity| identity.uuid != state.subvol) {
        bail!("tree btrfs identity differs from the signed state");
    }
    let mut base = None;
    let mut temporary = None;
    for parent in state.parents.iter().chain(state.derived_from.iter()) {
        if let Some(attempt) = states.get(parent).filter(|_| repo.p.has_state(parent)) {
            let (_, _, parent_tree) = reader.tree(parent, attempt)?;
            temporary =
                Some(crate::repo::Temp::source(&repo.state_src(parent)?, stage.clone(), crate::fs::Protection::Writable)?);
            base = Some(parent_tree);
            break;
        }
    }
    let temporary = match temporary {
        Some(temporary) => temporary,
        None => crate::repo::Temp::empty(stage.clone())?,
    };

    let result = (|| -> Result<usize> {
        let n = reader.restore_over(&tree, base.as_ref(), &stage)?;
        let published = publish_restored(repo, state, &stage, header.btrfs.as_ref().map_or(0, |i| i.transid))?;
        Ok(if published { n } else { 0 })
    })();
    match result {
        Ok(made) => {
            temporary.discard()?;
            Ok(made)
        }
        Err(error) => Err(temporary.abort(error)),
    }
}

/// Restore the listed states that are not on this machine, from the store's own trees. The
/// machine keys come from this project's trusted records, so no record segment is read.
pub(super) fn restore_states(store: &Store, repo: &Repo, ids: &[String]) -> Result<usize> {
    if store.is_api() {
        let mut states = Vec::new();
        for id in ids.iter().filter(|id| !repo.p.has_state(id)) {
            states.push(repo.state(id)?);
        }
        let mut report = Report::default();
        super::cloud_objects::fetch_states(store, repo, states, &mut report)?;
        return Ok(report.files);
    }
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    let mut keys = BTreeMap::new();
    for (machine, hex_key) in repo.p.db.trusted_keys()? {
        let bytes: [u8; 32] =
            hex::decode(&hex_key)?.try_into().map_err(|_| anyhow::anyhow!("trusted key of {machine} is not 32 bytes"))?;
        keys.insert(machine, VerifyingKey::from_bytes(&bytes)?);
    }
    let states = restore_attempts(store, repo, ids)?;
    let mut reader = Reader::new(&store.backend, &identity, &keys);
    let mut files = 0;
    for id in ids {
        if repo.p.has_state(id) {
            continue;
        }
        if !states.contains_key(id) {
            bail!("the store has no tree of state {id}");
        }
        let state = repo.state(id)?;
        files += restore_one(repo, &mut reader, &states, &state)?;
    }
    Ok(files)
}

/// Source heads for a new working copy, before it has a local line perspective.
pub(super) fn clone_lines(store: &Store, repo: &Repo) -> Result<Vec<crate::model::Line>> {
    if repo.p.is_overlay() {
        return repo.p.db.lines();
    }
    let lines = crate::sync_local::fetch::source_lines(repo, !store.is_api())?;
    let Some(api) = store.backend.api() else {
        return Ok(lines);
    };
    // V1 refs are the server's published positions, including a merge-job result.
    // The record snapshot must contain each target before a working copy can use it.
    let mut known: BTreeMap<_, _> = lines.into_iter().map(|line| (line.id.clone(), line)).collect();
    let mut selected = Vec::new();
    for reference in api.refs(&store.api_scope(), "lines/")? {
        let Some(target) = reference.target else { continue };
        let id = reference.name.strip_prefix("lines/").context("invalid line ref")?;
        let mut line = known.remove(id).with_context(|| format!("remote line {id} has no fetched metadata; fetch again"))?;
        let state =
            repo.p.db.state(&target)?.with_context(|| format!("remote line {id} target has no fetched record; fetch again"))?;
        anyhow::ensure!(!state.kind.auxiliary(), "remote line {id} targets an auxiliary state");
        line.head = target;
        selected.push(line);
    }
    Ok(selected)
}

pub(super) fn fetch(store: &mut Store, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
    fetch_inner(store, ctx, repo, opts, None)
}

/// The fetch of an overlay layer: `parent` is the project, for the selections that name its lines.
pub(super) fn fetch_layer(store: &mut Store, ctx: &Ctx, repo: &mut Repo, opts: &Options, parent: &Repo) -> Result<Report> {
    fetch_inner(store, ctx, repo, opts, Some(parent))
}

fn fetch_inner(store: &mut Store, ctx: &Ctx, repo: &mut Repo, opts: &Options, parent: Option<&Repo>) -> Result<Report> {
    if store.is_api() {
        fetch_legacy_api(store, ctx, repo, opts, parent)
    } else {
        let mut local = crate::sync_local::Local::write(ctx, repo, parent);
        fetch_with_local(store, &mut local, opts)
    }
}

// Compatibility adapter for the separate Cloud API's object-store codec.
fn fetch_legacy_api(store: &mut Store, ctx: &Ctx, repo: &mut Repo, opts: &Options, parent: Option<&Repo>) -> Result<Report> {
    let _fetch = crate::trace::span("store fetch");
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, opts.paper_key.as_deref())?;
    let local: Vec<String> = repo.p.export_jsonl()?.lines().map(str::to_string).collect();
    let records = records_with_local(&store.backend, &identity, &descriptor.project, &local, Some(&seen_file(&store.dir)))?;
    let bootstrap = repo.p.id.is_empty();
    if !bootstrap {
        for (machine, key) in &records.keys {
            if repo.p.db.trusted(machine)?.as_deref().is_some_and(|trusted| trusted != hex::encode(key.to_bytes())) {
                bail!("remote machine {machine}: first-record key does not match the trusted public key");
            }
        }
    }
    let count;
    {
        let _lock = repo.p.lock_maintenance()?;
        // On the Layr Cloud API the server access list decides who writes, and it checked each signature against
        // the key that the writer registered; local machine trust is not asked and does not grow
        // (docs/cloud-rfc.md 7.0). Each writer key is pinned per remote on first sight.
        pin_writer_keys(&store.dir, &records.keys)?;
        count = _lock.import_lines(&records.lines, crate::store::Trust::Server)?;
    }
    if bootstrap {
        repo.p.reload_id()?;
    }
    store.project = repo.p.id.clone();
    if repo.p.id != descriptor.project {
        bail!("the records do not belong to store.json's project");
    }
    let pending = store.dir.join("pending-lines.json");
    let pending_clone =
        if pending.exists() { !serde_json::from_slice::<BTreeSet<String>>(&std::fs::read(pending)?)?.is_empty() } else { false };
    let clone_lines = if bootstrap || pending_clone { Some(clone_lines(store, repo)?) } else { None };
    let states = api_states(repo)?;
    let plan = crate::sync_local::Local::write(ctx, repo, parent).select_fetch(
        &records.lines,
        opts,
        &states.keys().cloned().collect::<Vec<_>>(),
        clone_lines,
    )?;
    let mut report = Report {
        clone_lines: plan.clone_lines,
        omitted: plan.omitted,
        records: count,
        overlaps: records.overlaps,
        gaps: records.gaps.clone(),
        ..Default::default()
    };
    report.git_packs = super::gitpack::fetch(store, &crate::sync_local::Local::read(ctx, repo), &identity)?;
    let api = store.backend.api().context("API session missing")?;
    let scope = store.api_scope();
    api.learn_states(&scope, &plan.missing.iter().map(|s| s.id.clone()).collect::<Vec<_>>())?;
    let mut missing = Vec::new();
    for state in plan.missing {
        if api.state_held(&scope, &state.id)? {
            missing.push(state);
        } else if !plan.restricted {
            report.omitted.push(format!("state {}: not on Layr Cloud", crate::ids::short(&state.id)));
        }
    }
    super::cloud_objects::fetch_states(store, repo, missing, &mut report)?;
    record_left(store, opts, plan.left, &mut report)?;
    // Trust the chosen store's machines only during the initial clone, as peer bootstrap does. A Layr Cloud clone
    // trusts only the project's own trust chain (`project_trust_chain`): the server access list is not local trust.
    if bootstrap {
        let chain = project_trust_chain(&records.lines, &records.keys)?;
        for (machine, key) in &records.keys {
            if !chain.contains(machine) {
                continue;
            }
            if repo.p.db.trusted(machine)?.is_none() {
                let mut record = crate::model::DraftRecord::new("machine.trust");
                record.data = serde_json::json!({"machine":machine,"public_key":hex::encode(key.to_bytes())});
                repo.commit_record(&repo.p.lock_maintenance()?, ctx, record)?;
            }
        }
    }
    Ok(report)
}

/// The v2 engine consumes only the local replica port. API object storage retains its named legacy path.
pub(crate) fn fetch_with_local(store: &mut Store, local: &mut super::Replica<'_>, opts: &Options) -> Result<Report> {
    let _fetch = crate::trace::span("store fetch");
    let started = store.backend.transfer_stats();
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, opts.paper_key.as_deref())?;
    let mut records_local = Vec::new();
    local.visit_records(None, &mut |line| {
        records_local.push(line.to_owned());
        Ok(())
    })?;
    let records =
        records_with_local(&store.backend, &identity, &descriptor.project, &records_local, Some(&seen_file(&store.dir)))?;
    let imported = local.import_records(&records.lines, &records.keys)?;
    store.project = imported.project;
    if store.project != descriptor.project {
        bail!("the records do not belong to store.json's project");
    }
    let bootstrap = imported.bootstrap;
    let pending = store.dir.join("pending-lines.json");
    let pending_clone =
        if pending.exists() { !serde_json::from_slice::<BTreeSet<String>>(&std::fs::read(pending)?)?.is_empty() } else { false };
    let mut report =
        Report { records: imported.count, overlaps: records.overlaps, gaps: records.gaps.clone(), ..Default::default() };
    let preflight = local.fetch_preflight(&records.lines, opts, bootstrap || pending_clone)?;
    // Only previously confirmed remote omissions may be reused without a listing.
    let confirmed = std::fs::read_to_string(super::states_left_file(&store.dir)).unwrap_or_default();
    let mut listed = Vec::new();
    let (states, plan) = match preflight.filter(|p| all_confirmed(&confirmed, &p.left)) {
        Some(plan) => (BTreeMap::new(), plan),
        None => {
            listed = store.backend.list("states/")?;
            let states = attempts_of(&listed);
            let plan = local.fetch_selection(
                &records.lines,
                opts,
                &states.keys().cloned().collect::<Vec<_>>(),
                bootstrap || pending_clone,
            )?;
            (states, plan)
        }
    };
    report.git_packs = super::gitpack::fetch(store, local, &identity)?;
    report.clone_lines = plan.clone_lines;
    report.omitted = plan.omitted;
    let left = plan.left;
    let missing = plan.missing;
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    reader.chunk_dir = Some(store.dir.clone());
    reader.cache_in(local.fetch_temp_dir());
    reader.sizes = listed.into_iter().filter_map(|object| Some((object.key, object.size?))).collect();
    record_left(store, opts, left, &mut report)?;
    let missing = super::writer::parents_first(missing)?;
    // Cache states that failed to restore, with the packs of the remote then (`<state> <packs>` lines): one is tried
    // again only when a push or a release changed the packs, so an idle fetch does not download it each time.
    let failed_path = store.dir.join("cache-restore-failed");
    let mut failed: BTreeMap<String, String> = std::fs::read_to_string(&failed_path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once(' ').map(|(a, b)| (a.to_string(), b.to_string())))
        .collect();
    let failed_before = failed.clone();
    let mut packs_now: Option<String> = None;
    let mut pending_states = Vec::new();
    for state in missing {
        let id = state.id.clone();
        let cache = local.is_overlay() && state.cache_of.is_some();
        if cache && failed.contains_key(&id) {
            let now = match &packs_now {
                Some(now) => now.clone(),
                None => packs_now.insert(super::chunked::packs_fingerprint(&store.backend)?).clone(),
            };
            if failed.get(&id) == Some(&now) {
                report.omitted.push(format!(
                    "cache state {}: not restored (it failed before, and the remote did not change)",
                    crate::ids::short(&id)
                ));
                continue;
            }
        }
        pending_states.push(state);
    }
    let missing = pending_states;
    if reader.material.is_some() && missing.iter().all(|state| state.cache_of.is_none()) {
        let mut roots = Vec::new();
        let mut bases = Vec::new();
        let mut earlier = BTreeSet::new();
        for state in &missing {
            roots.push((state.id.clone(), states[&state.id].clone()));
            // Earlier selected states will be present at this restore. Among
            // those and the adapter's current base, only the first can be used.
            let current = local.restore_base(state, &states);
            let base = state
                .parents
                .iter()
                .chain(&state.derived_from)
                .find(|id| earlier.contains(*id) || current.as_ref() == Some(*id))
                .cloned();
            earlier.insert(state.id.clone());
            roots.extend(base.iter().map(|id| (id.clone(), states[id].clone())));
            bases.push(base);
        }
        reader.prefetch_trees(roots)?;
        for (state, base) in missing.iter().zip(bases) {
            let (_, _, tree) = reader.tree(&state.id, &states[&state.id])?;
            let base = base.as_ref().map(|id| reader.tree(id, &states[id]).map(|(_, _, tree)| tree)).transpose()?;
            for (path, entry) in &tree.files {
                if tree.chunks.contains_key(path) || unchanged(&tree, base.as_ref(), path, entry) {
                    continue;
                }
                for link in entry.3.iter().rev() {
                    let key = link.object();
                    let large = link.3.saturating_add(link.4) >= 16 << 20;
                    if let Some((_, uses, was_large)) = reader.part_uses.get_mut(&key) {
                        *uses += 1;
                        *was_large |= large;
                    } else {
                        reader.part_uses.insert(key.clone(), (reader.part_order.len(), 1, large));
                        reader.part_order.push(key);
                    }
                }
            }
        }
    }
    if missing.iter().any(|state| state.cache_of.is_some()) {
        // Cache deletion races remain per-state omissions. A partially successful
        // batch still leaves reusable material for the states that survived.
        // Authentication/server errors are retried by demand reads and reported there.
        let roots = missing.iter().map(|state| (state.id.clone(), states[&state.id].clone())).collect();
        let _ = reader.prefetch_trees(roots);
    }
    for state in missing {
        let id = state.id.clone();
        let cache = local.is_overlay() && state.cache_of.is_some();
        let files = match restore_with_local(local, &mut reader, &states, &state) {
            Ok(files) => {
                failed.remove(&id);
                files
            }
            // A cache state that a push removed while this fetch ran (its sweep deletes parts, then the tree, of a
            // cache that a newer state replaced): the cache is regenerable, so it omits, and the fetch goes on.
            Err(e) if cache => {
                report.omitted.push(format!("cache state {}: not restored ({e:#})", crate::ids::short(&id)));
                let now = match &packs_now {
                    Some(now) => now.clone(),
                    None => packs_now.insert(super::chunked::packs_fingerprint(&store.backend)?).clone(),
                };
                failed.insert(id, now);
                continue;
            }
            Err(e) => return Err(e),
        };
        report.files += files;
        report.states += 1;
        report.state_ids.push(id.clone());
    }
    if failed != failed_before {
        let text: String = failed.iter().map(|(state, packs)| format!("{state} {packs}\n")).collect();
        crate::fsutil::atomic_write(&failed_path, text.as_bytes(), 0o600)?;
    }
    // Trust the chosen store's machines only during the initial clone, as peer bootstrap does. A Layr Cloud clone
    // trusts only the project's own trust chain (`project_trust_chain`): the server access list is not local trust.
    if bootstrap {
        local.finish_import(&records.keys)?;
        // A Private or Public store has no vault key, so there is nothing to seal in a slot.
        if store.layer.is_none() && !store.is_public_reader() && descriptor.mode.is_secret() {
            let slot = keys::machine_slot(&identity, &store.key.verifying_key())?;
            // Optional enrollment keeps key metadata outside shared history: it supplies
            // a public key for resealing this existing reader slot, never writer trust.
            let enroll = || -> Result<()> {
                if let Some(line) = local.record_line(&store.machine, 1)? {
                    let record = oplog::parse_line(&line)?.record;
                    if record.op != "machine" {
                        bail!("local machine announcement missing");
                    }
                    store.backend.put_new(
                        &format!("slots/machine-{}.key.age", store.machine),
                        keys::encrypt(&descriptor.recipient, line.as_bytes())?,
                    )?;
                    store.backend.put_new(&format!("slots/machine-{}.age", store.machine), slot)?;
                }
                Ok(())
            };
            let _ = enroll();
        }
    }
    report.bytes = store.backend.transfer_stats().downloaded.saturating_sub(started.downloaded);
    Ok(report)
}

/// Whether every state of `left` is in `confirmed` (the lines of the `states-left` file).
fn all_confirmed(confirmed: &str, left: &[String]) -> bool {
    let confirmed: std::collections::HashSet<&str> = confirmed.lines().collect();
    left.iter().all(|id| confirmed.contains(id.as_str()))
}

fn record_left(store: &Store, opts: &Options, left: Vec<String>, report: &mut Report) -> Result<()> {
    if !left.is_empty() {
        // A read of such a state restores it from this remote (`restore_state`).
        let path = super::states_left_file(&store.dir);
        let mut all: BTreeSet<String> = std::fs::read_to_string(&path).unwrap_or_default().lines().map(str::to_owned).collect();
        all.extend(left.iter().cloned());
        crate::fsutil::atomic_write(&path, format!("{}\n", all.into_iter().collect::<Vec<_>>().join("\n")).as_bytes(), 0o600)?;
        let why = if opts.lines.is_empty() && opts.overlays.is_none() { "--heads" } else { "--line, --overlay" };
        report.omitted.push(format!("{} state(s) of the history: on the remote, restored on first read ({why})", left.len()));
    }
    Ok(())
}

fn restore_with_local(
    local: &mut super::Replica<'_>,
    reader: &mut Reader<'_>,
    states: &BTreeMap<String, String>,
    state: &crate::model::StateRec,
) -> Result<usize> {
    let (header, _, tree) = reader.tree(&state.id, &states[&state.id])?;
    if header.btrfs.as_ref().is_some_and(|identity| identity.uuid != state.subvol) {
        bail!("tree btrfs identity differs from the signed state");
    }
    let parent = local.restore_base(state, states);
    let base = parent.as_ref().map(|id| reader.tree(id, &states[id]).map(|(_, _, tree)| tree)).transpose()?;
    let staged = |local: &mut super::Replica<'_>, reader: &mut Reader<'_>| {
        let restored = local.stage_state(parent.as_deref()).and_then(|stage| reader.restore_over(&tree, base.as_ref(), &stage));
        restored.map_err(|error| local.abort_state(error))
    };
    let restored = match staged(local, reader) {
        // The operation cache shares the disk: it goes, and the state is staged and written once more without it.
        Err(error) if crate::fs::is_storage_full(&error) && reader.material.take().is_some() => staged(local, reader),
        restored => restored,
    };
    local.install_state(state, header.btrfs.as_ref().map_or(0, |identity| identity.transid), restored?)
}

pub(super) fn verify(store: &Store, files: bool) -> Result<Report> {
    if store.is_api() {
        bail!("Layr Cloud checks each object against its id on upload; verify is for store folders and buckets");
    }
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    let records = records(&store.backend, &identity, &descriptor.project)?;
    let mut report =
        Report { records: records.lines.len(), overlaps: records.overlaps, gaps: records.gaps.clone(), ..Default::default() };
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    let known: BTreeMap<String, String> = records
        .lines
        .iter()
        .map(|line| oplog::parse_line(line).expect("checked").record)
        .flat_map(|r| r.states.into_iter().map(|s| (s.id, s.subvol)))
        .collect();
    for object in store.backend.list("states/")? {
        if let Some((state, attempt)) = format::parse_tree_name(&object.key) {
            if !known.contains_key(&state) {
                bail!("remote tree {state} has no state record");
            }
            let (header, _, tree) = reader.tree(&state, &attempt)?;
            if header.btrfs.as_ref().is_some_and(|identity| Some(&identity.uuid) != known.get(&state)) {
                bail!("tree btrfs identity differs from the signed state");
            }
            for part in 0..header.parts {
                if store.backend.get(&format::data_name(&state, &attempt, part))?.is_none() {
                    bail!("tree has a missing data part");
                }
            }
            report.states += 1;
            if files {
                for path in tree.files.keys() {
                    reader.read_file(&tree, path)?;
                    report.files += 1;
                }
            }
        }
    }
    Ok(report)
}

pub(super) fn log(store: &Store) -> Result<Vec<String>> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    Ok(records(&store.backend, &identity, &descriptor.project)?.lines)
}

#[cfg(test)]
thread_local! {
    /// The clock of `gc` (unix seconds), and a step that runs after its scan of the trees: tests move the clock there.
    pub(super) static GC_CLOCK: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
    pub(super) static GC_AFTER_SCAN: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

fn gc_now() -> i64 {
    #[cfg(test)]
    if let Some(now) = GC_CLOCK.with(std::cell::Cell::get) {
        return now;
    }
    chrono::Utc::now().timestamp()
}

pub(super) fn gc(store: &Store, dry_run: bool) -> Result<Vec<String>> {
    if store.is_api() {
        bail!("Layr Cloud collects garbage on the server; a client does not sweep");
    }
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    store.require_private_access()?;
    let records = records(&store.backend, &identity, &descriptor.project)?;
    if !records.gaps.is_empty() {
        bail!("remote record gaps prevent collection");
    }
    // A day before the listing: a writer publishes within 23 hours of its oldest unpublished part, so a part younger
    // than that at the listing can be linked by a tree that the listing does not show, however long the scan takes.
    let cutoff = gc_now() - 86400;
    let objects = store.backend.list("states/")?;
    let mut referenced = BTreeSet::new();
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    // A complete signed tree protects every frame it references, including cross-line reuse.
    for object in &objects {
        if let Some((state, attempt)) = format::parse_tree_name(&object.key) {
            let (_, _, tree) = reader.tree(&state, &attempt)?;
            for entry in tree.files.values() {
                for link in &entry.3 {
                    referenced.insert(link.object());
                }
            }
        }
    }
    #[cfg(test)]
    GC_AFTER_SCAN.with(|hook| hook.borrow_mut().as_mut().map(|hook| hook()));
    let mut deleted = Vec::new();
    // Temporary files of puts that stopped: no listing shows them.
    deleted.extend(store.backend.stale_temporaries(cutoff, dry_run)?);
    let release_objects = store.backend.list("files/")?;
    if !release_objects.is_empty() {
        let model = super::release::model(&descriptor.project, &records, &store.machine)?;
        let (live, known) = model.release_objects(crate::ids::now_ms())?;
        for object in release_objects {
            if !layr_model::release::object(&object.key) {
                bail!("invalid release object in store");
            }
            // A recent unattached upload may still be in progress. Published files become
            // collectible immediately after their release ref is deleted.
            if live.contains(&object.key) || (!known.contains(&object.key) && object.modified >= cutoff) {
                continue;
            }
            if !dry_run {
                store.backend.delete(&object.key)?;
            }
            deleted.push(object.key);
        }
    }
    for object in objects {
        let Some((state, attempt, _)) = format::parse_data_name(&object.key) else {
            continue;
        };
        if object.modified >= cutoff || referenced.contains(&object.key) {
            continue;
        }
        // Writers stop at +23h, using the earliest of the new attempt start and
        // server modified times of reused unpublished parts. Pending hashes retain
        // those times across crashes until a referencing tree publishes. Thus an
        // unreferenced part older than 24h cannot acquire a new published reference.
        if store.backend.get(&format::tree_name(&state, &attempt))?.is_some() {
            continue;
        }
        if !dry_run {
            store.backend.delete(&object.key)?;
        }
        deleted.push(object.key);
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    const MACHINE: &str = "01a12290-0000-7000-8000-000000000001";
    const PROJECT: &str = "01a12290-0000-7000-8000-000000000002";
    const STATE: &str = "01a12290-0000-7000-8000-000000000003";

    #[test]
    fn restoring_local_states_skips_attempts_but_a_missing_state_still_lists() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let local = crate::store::Store::open(&root.join("local")).unwrap();
        let dir = local.project_dir("p").unwrap();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let repo = Repo {
            restore_from_remote: true,
            p: crate::store::Project {
                lease: Default::default(),
                store: &local,
                name: "p".into(),
                dir: dir.clone(),
                id: PROJECT.into(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
            ctx: None,
        };
        let store = super::super::open(&format!("file://{}", root.join("remote").display()), &repo).unwrap();
        let attempt = "deadbeef";
        store.backend.put_new(&format::tree_name(STATE, attempt), vec![]).unwrap();
        std::fs::create_dir_all(repo.p.state_path(STATE)).unwrap();
        assert!(restore_attempts(&store, &repo, &[STATE.into()]).unwrap().is_empty());
        std::fs::remove_dir(repo.p.state_path(STATE)).unwrap();
        assert_eq!(restore_attempts(&store, &repo, &[STATE.into()]).unwrap(), BTreeMap::from([(STATE.into(), attempt.into())]));
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_ancestry_is_read_once_and_includes_every_parent() {
        let mut calls = BTreeMap::<String, usize>::new();
        let mut tips = vec!["missing".to_string()];
        tips.extend((0..400).map(|n| format!("branch-{n}")));
        tips.push("branch-0".into());
        let parents = |id: &str| {
            if id.starts_with("branch-") {
                vec!["shared-999".into(), "derived".into()]
            } else if let Some(number) = id.strip_prefix("shared-").and_then(|n| n.parse::<u32>().ok()) {
                if number == 0 {
                    vec![]
                } else {
                    vec![format!("shared-{}", number - 1)]
                }
            } else {
                vec![]
            }
        };
        let mut previous = BTreeSet::new();
        let mut previous_reads = 0;
        for tip in &tips {
            previous.extend(
                walk_ancestry([tip.clone()], |id| {
                    previous_reads += 1;
                    Ok(parents(id))
                })
                .unwrap(),
            );
        }
        let found = walk_ancestry(tips, |id| {
            *calls.entry(id.to_string()).or_default() += 1;
            Ok(parents(id))
        })
        .unwrap();
        assert_eq!(found.iter().cloned().collect::<BTreeSet<_>>(), previous);
        assert_eq!(found.len(), 1402);
        assert_eq!(calls.len(), 1402);
        assert!(calls.values().all(|&count| count == 1));
        assert!(previous_reads > 400_000);
        eprintln!("ancestry reads: previous {previous_reads}, combined {}", calls.len());
        assert!(found.contains(&"missing".to_string()));
        assert!(found.contains(&"derived".to_string()));
    }

    fn fixture() -> (Backend, String, ed25519_dalek::SigningKey, std::path::PathBuf) {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        (Backend::file(root.clone()), keys::generate_identity(), ed25519_dalek::SigningKey::from_bytes(&[9; 32]), root)
    }
    fn state_id(n: usize) -> String {
        format!("01a12290-0000-7000-8000-{n:012x}")
    }
    fn write_tree(
        backend: &Backend,
        key: &ed25519_dalek::SigningKey,
        state: &str,
        base: Option<(&str, &[u8])>,
        tree: &Tree,
    ) -> Vec<u8> {
        let canonical = tree.canonical().unwrap();
        let mut header = Header {
            v: 2,
            state: state.into(),
            attempt: "0123abcd".into(),
            tree: if base.is_some() { "delta" } else { "full" }.into(),
            base: base.map(|(id, _)| (id.into(), "0123abcd".into())),
            parts: 1,
            machine: MACHINE.into(),
            btrfs: None,
            sig: String::new(),
        };
        header.sign(&canonical, key).unwrap();
        let mut object = serde_json::to_vec(&header).unwrap();
        object.push(b'\n');
        object.extend(zst::compress(&canonical, base.map(|(_, bytes)| bytes)).unwrap());
        backend.put_new(&format::tree_name(state, "0123abcd"), object).unwrap();
        canonical
    }

    #[test]
    fn pipeline_heads_only_closure_shares_32_level_bases_and_rejects_bad_data() {
        let (files, _, key, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let keys = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
        let tree = Tree::default();
        let mut previous = Vec::new();
        for n in 10..41 {
            previous = write_tree(
                &files,
                &key,
                &state_id(n),
                (n > 10).then(|| (state_id(n - 1), previous.as_slice())).as_ref().map(|(id, bytes)| (id.as_str(), *bytes)),
                &tree,
            );
        }
        for n in 41..43 {
            write_tree(&files, &key, &state_id(n), Some((&state_id(40), &previous)), &tree);
        }
        // An unrelated loose tree must stay outside heads-only closure.
        write_tree(&files, &key, &state_id(99), None, &tree);
        let (backend, _peer) = super::super::gateway::tests::file_transport(files.clone());
        let before = backend.transfer_stats();
        let mut reader = Reader::new(&backend, keys::PLAIN, &keys);
        std::fs::create_dir_all(&root).unwrap();
        reader.material = Material::new(root.join("material"));
        reader.prefetch_trees(vec![(state_id(41), "0123abcd".into()), (state_id(42), "0123abcd".into())]).unwrap();
        assert_eq!(reader.get_count(), 33);
        assert_eq!(backend.transfer_stats().requests - before.requests, 32);
        assert_eq!(reader.tree(&state_id(41), "0123abcd").unwrap().2, tree);
        assert_eq!(reader.tree_depth(&state_id(41), "0123abcd").unwrap(), 31);
        assert_eq!(reader.get_count(), 33);
        let before = backend.transfer_stats();
        let mut history = Reader::new(&backend, keys::PLAIN, &keys);
        history.material = Material::new(root.join("history"));
        history.prefetch_trees((10..43).map(|n| (state_id(n), "0123abcd".into())).collect()).unwrap();
        assert_eq!(backend.transfer_stats().requests - before.requests, 1, "selected history collapses dependency rounds");
        history.tree(&state_id(41), "0123abcd").unwrap();
        history.tree(&state_id(42), "0123abcd").unwrap();
        assert_eq!(history.get_count(), 33, "shared bases load once");
        // Mutation of signed content still fails after the unauthenticated routing pass.
        let mut bad = serde_json::to_vec(&Header {
            v: 2,
            state: state_id(98),
            attempt: "0123abcd".into(),
            tree: "full".into(),
            base: None,
            parts: 0,
            machine: MACHINE.into(),
            btrfs: None,
            sig: "bad".into(),
        })
        .unwrap();
        bad.push(b'\n');
        bad.extend(zst::compress(&previous, None).unwrap());
        files.put_new(&format::tree_name(&state_id(98), "0123abcd"), bad).unwrap();
        reader.prefetch_trees(vec![(state_id(98), "0123abcd".into())]).unwrap();
        assert!(reader.tree(&state_id(98), "0123abcd").is_err());
        files.put_new(&format::tree_name(&state_id(97), "0123abcd"), b"bad header".to_vec()).unwrap();
        assert!(reader.prefetch_trees(vec![(state_id(97), "0123abcd".into())]).is_err());
    }
    #[test]
    fn pipeline_rejects_delta_cycles_and_bad_frames_before_publication() {
        let (backend, _, key, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let keys = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
        let bytes = Tree::default().canonical().unwrap();
        write_tree(&backend, &key, &state_id(10), Some((&state_id(11), &bytes)), &Tree::default());
        write_tree(&backend, &key, &state_id(11), Some((&state_id(10), &bytes)), &Tree::default());
        let mut reader = Reader::new(&backend, keys::PLAIN, &keys);
        std::fs::create_dir_all(&root).unwrap();
        reader.material = Material::new(root.join("material"));
        reader.prefetch_trees(vec![(state_id(10), "0123abcd".into())]).unwrap();
        assert!(reader.tree(&state_id(10), "0123abcd").unwrap_err().to_string().contains("loop"));
        let mut object = backend.get(&format::tree_name(&state_id(10), "0123abcd")).unwrap().unwrap();
        let (mut header, _) = format::split_tree(&object).unwrap();
        header.state = state_id(12);
        header.tree = "full".into();
        header.base = None;
        header.sign(&bytes, &key).unwrap();
        object = serde_json::to_vec(&header).unwrap();
        object.push(b'\n');
        object.extend_from_slice(b"invalid zstd frame");
        backend.put_new(&format::tree_name(&state_id(12), "0123abcd"), object).unwrap();
        reader.prefetch_trees(vec![(state_id(12), "0123abcd".into())]).unwrap();
        assert!(reader.tree(&state_id(12), "0123abcd").is_err());
    }

    #[test]
    fn pipeline_batches_only_uncached_chunk_indexes_and_checks_live_pairs() {
        use layr_store::chunks;
        let (files, _, _, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let cache = root.join("cache");
        for n in 10..30 {
            let id = state_id(n);
            let mut builder = chunks::Builder::default();
            let data = [u8::try_from(n).unwrap()];
            builder.add(hex::encode(ContentAddress::digest(data)), &data);
            let (body, index) = builder.finish().unwrap();
            files.put_new(&chunks::pack_name(&id), body).unwrap();
            files.put_new(&chunks::index_name(&id), index.encode()).unwrap();
        }
        let (backend, _peer) = super::super::gateway::tests::file_transport(files.clone());
        let before = backend.transfer_stats();
        let first = super::super::chunked::indexes(&backend, Some(&cache), keys::PLAIN).unwrap();
        assert_eq!(first.len(), 20);
        assert_eq!(backend.transfer_stats().requests - before.requests, 2, "one listing and one index batch");
        let warm = backend.transfer_stats();
        assert_eq!(super::super::chunked::indexes(&backend, Some(&cache), keys::PLAIN).unwrap(), first);
        assert_eq!(backend.transfer_stats().requests - warm.requests, 1, "warm indexes need only the pair listing");
        assert_eq!(backend.transfer_stats().downloaded, warm.downloaded);
        std::fs::write(cache.join("chunk-indexes").join(state_id(10)), b"bad cache").unwrap();
        let before = backend.transfer_stats();
        assert_eq!(super::super::chunked::indexes(&backend, Some(&cache), keys::PLAIN).unwrap(), first);
        assert_eq!(backend.transfer_stats().requests - before.requests, 2);
        files.delete(&chunks::pack_name(&state_id(10))).unwrap();
        let alive = super::super::chunked::indexes(&backend, Some(&cache), keys::PLAIN).unwrap();
        assert_eq!(alive.len(), 19, "a cached index cannot make a deleted pack live");
        assert!(!cache.join("chunk-indexes").join(state_id(10)).exists());
        eprintln!("20 chunk indexes: 20 individual GETs -> 1 POST batch; warm -> 0 body requests");
    }

    /// A heads-only fetch of a long history checks its omitted states against the confirmed ones in one pass.
    #[test]
    fn confirmed_omissions_are_checked_in_linear_time() {
        let left: Vec<String> = (0..50_000).map(state_id).collect();
        let confirmed = format!("{}\n", left.iter().rev().cloned().collect::<Vec<_>>().join("\n"));
        let started = std::time::Instant::now();
        assert!(all_confirmed(&confirmed, &left));
        assert!(!all_confirmed(&confirmed, &[state_id(50_000)]));
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
    }

    /// A window of small parts is one run of the transfer pool and holds at most half of the operation cache. With the
    /// listed sizes of the parts it takes up to 256 small parts, read several at a time; without them, the parts that
    /// would fit as full parts, read one at a time (a File or S3 body of unknown size has no ceiling).
    #[test]
    fn a_part_window_runs_once_on_the_pool_within_half_the_cache() {
        let (backend, _, _, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let keys = BTreeMap::new();
        let window = window_parts();
        assert!(window as u64 * keys::ciphertext_len(format::PART_LIMIT) as u64 <= fetch_plan::MATERIAL_BYTES / 2);
        let mut entries = Vec::new();
        for n in 0..window + 10 {
            let content = format!("part {n}\n").into_bytes();
            let frame = zst::compress(&content, None).unwrap();
            let id = state_id(n);
            backend.put_new(&format::data_name(&id, "0123abcd", 0), frame.clone()).unwrap();
            let link = format::Link(id, "0123abcd".into(), 0, 0, frame.len() as u64, "full".into());
            entries.push(Entry("100644".into(), content.len() as u64, hex::encode(ContentAddress::digest(&content)), vec![link]));
        }
        std::fs::create_dir_all(&root).unwrap();
        // The parts whose size the listing lacks: none, the last one, all.
        for unknown in [0, 1, entries.len()] {
            let mut reader = Reader::new(&backend, keys::PLAIN, &keys);
            reader.material = Material::new(root.join(format!("material-{unknown}")));
            for entry in &entries {
                reader.part_uses.insert(entry.3[0].object(), (reader.part_order.len(), 1, false));
                reader.part_order.push(entry.3[0].object());
            }
            let listed = &reader.part_order[..entries.len() - unknown];
            reader.sizes = backend
                .list("states/")
                .unwrap()
                .into_iter()
                .filter(|o| listed.contains(&o.key))
                .map(|o| (o.key, o.size.unwrap()))
                .collect();
            let threads = || super::super::backend::FILE_THREADS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            threads().insert(root.clone(), Default::default());
            reader.read(&entries[0]).unwrap();
            let readers = threads().remove(&root).unwrap_or_default().len();
            if unknown < entries.len() {
                assert_eq!(reader.get_count(), entries.len(), "one window of every listed part and one full part");
                // The part of unknown size reads alone; the others still share the pool.
                assert!(readers > 1 && readers <= super::super::download::JOBS, "{unknown} unknown: {readers} reader threads");
            } else {
                assert_eq!(reader.get_count(), window, "one window of full parts");
                assert_eq!(readers, 1, "reads of unknown size run alone");
            }
        }
    }

    #[test]
    #[ignore = "subprocess helper for an_understated_size_fails_before_the_body_is_held"]
    fn understated_size_process() {
        use super::super::writer::tests::large_blocks;
        use std::sync::atomic::Ordering::SeqCst;
        if std::env::var_os("LAYR_TEST_PART_MEMORY").is_none() {
            return;
        }
        let (backend, _, _, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        backend.put_new("states/x", vec![7; 32 << 20]).unwrap();
        large_blocks::ON.store(true, SeqCst);
        let read = backend.visit_many(&["states/x".into()], &[Some(1 << 20)], |_, _| Ok(()));
        large_blocks::ON.store(false, SeqCst);
        assert!(format!("{:#}", read.unwrap_err()).contains("exceeds"));
        println!("part memory {}", large_blocks::MOST.load(SeqCst));
    }

    /// An object that its listing says is 1 MiB and that holds 32 MiB: the read fails without taking in more than the
    /// listed size.
    #[test]
    fn an_understated_size_fails_before_the_body_is_held() {
        let most = super::super::writer::tests::large_blocks_of("store_remote::reader::tests::understated_size_process").unwrap();
        assert!(most <= 2 << 20, "{most} bytes held");
    }

    /// A body larger than its listed size fails its read.
    #[test]
    fn a_body_larger_than_its_listed_size_fails() {
        let (backend, _, _, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        backend.put_new("states/x", b"ten bytes!".to_vec()).unwrap();
        let read = |size| backend.visit_many(&["states/x".into()], &[Some(size)], |_, _| Ok(()));
        assert!(read(10).is_ok());
        assert!(read(5).unwrap_err().to_string().contains("exceeds its size bound"));
    }

    #[test]
    fn pipeline_reuses_oversized_parts_after_memory_eviction() {
        use rand::RngCore;
        let (backend, _, _, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        let keys = BTreeMap::new();
        let mut reader = Reader::new(&backend, keys::PLAIN, &keys);
        std::fs::create_dir_all(&root).unwrap();
        reader.material = Material::new(root.join("material"));
        let mut entries = Vec::new();
        for n in 0..2 {
            let mut content = vec![0; 80 << 20];
            rand::thread_rng().fill_bytes(&mut content);
            let frame = zst::compress(&content, None).unwrap();
            let id = state_id(n);
            backend.put_new(&format::data_name(&id, "0123abcd", 0), frame.clone()).unwrap();
            let entry = Entry(
                "100644".into(),
                content.len() as u64,
                hex::encode(ContentAddress::digest(&content)),
                vec![format::Link(id, "0123abcd".into(), 0, 0, frame.len() as u64, "full".into())],
            );
            reader.part_uses.insert(entry.3[0].object(), (reader.part_order.len(), 2, true));
            reader.part_order.push(entry.3[0].object());
            entries.push(entry);
        }
        for (n, entry) in entries.iter().enumerate() {
            reader.read(entry).unwrap();
            assert_eq!(reader.get_count(), n + 1, "uncapped 80 MiB objects are read exclusively, one at a time");
        }
        for entry in entries {
            reader.read(&entry).unwrap();
        }
        assert_eq!(reader.get_count(), 2, "160 MiB of unique parts must survive memory eviction");
        assert_eq!(reader.material.as_ref().unwrap().bytes, 0, "last planned use reclaims disk bytes");
    }

    fn line(seq: u64, key: &ed25519_dalek::SigningKey) -> String {
        let mut record = Record::untrusted_preview(if seq == 1 { "machine" } else { "config" });
        record.id = format!("01a12290-0000-7000-8000-{seq:012x}");
        record.machine = MACHINE.into();
        record.project = PROJECT.into();
        record.seq = seq;
        if seq == 1 {
            record.data = serde_json::json!({"name":"third","public_key":hex::encode(key.verifying_key().to_bytes())});
        }
        oplog::sign_line(&record, key).unwrap()
    }
    fn segment(backend: &Backend, identity: &str, first: u64, last: u64, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        assert!(backend
            .put_new(
                &format::segment_name(MACHINE, first, last),
                keys::encrypt(&keys::recipient_of(identity).unwrap(), body.as_bytes()).unwrap()
            )
            .unwrap());
    }
    /// Segments of just over 1 MiB: 30 records with a 35 KiB value each.
    const SEGMENTS: u64 = 26;
    const SEGMENT_BYTES: u64 = 30 * (100 << 10);

    #[test]
    #[ignore = "subprocess helper for record_segments_are_checked_as_they_come"]
    fn record_memory_process() {
        use super::super::writer::tests::large_blocks;
        use std::sync::atomic::Ordering::SeqCst;
        if std::env::var_os("LAYR_TEST_PART_MEMORY").is_none() {
            return;
        }
        let (backend, identity, key, root) = fixture();
        let _cleanup = crate::repo::Temp::plain(root.clone());
        segment(&backend, &identity, 1, 1, &[line(1, &key)]);
        for first in (0..SEGMENTS).map(|n| 2 + n * 30) {
            let lines: Vec<String> = (first..first + 30)
                .map(|seq| {
                    let mut record = oplog::parse_line(&line(seq, &key)).unwrap().record;
                    record.data = serde_json::json!({ "pad": "x".repeat(100 << 10) });
                    oplog::sign_line(&record, &key).unwrap()
                })
                .collect();
            segment(&backend, &identity, first, first + 29, &lines);
        }
        large_blocks::ON.store(true, SeqCst);
        let records = records_with_local(&backend, &identity, PROJECT, &[], None);
        large_blocks::ON.store(false, SeqCst);
        assert_eq!(records.unwrap().lines.len() as u64, 1 + SEGMENTS * 30);
        println!("part memory {}", large_blocks::MOST.load(SeqCst));
    }

    /// A clone checks each record segment as its read completes: the bodies of a long history are not all held.
    #[test]
    fn record_segments_are_checked_as_they_come() {
        let most = super::super::writer::tests::large_blocks_of("store_remote::reader::tests::record_memory_process").unwrap();
        eprintln!("{SEGMENTS} segments of 3 MiB: {} MiB of large blocks at most", most >> 20);
        // The reads in flight hold their bodies within the read budget (their listed sizes), and the segment being
        // checked its plaintext too. Before, every body stayed until the last one came.
        let segment = keys::ciphertext_len(usize::try_from(SEGMENT_BYTES).unwrap_or(usize::MAX)) as u64 + (64 << 10);
        assert!(SEGMENTS * segment > super::super::download::BYTE_BUDGET + 2 * segment, "all segments exceed the bound");
        assert!(most <= super::super::download::BYTE_BUDGET + 2 * segment, "{most} bytes held");
    }

    #[test]
    fn a_checked_segment_is_not_read_again_and_a_replaced_one_is() {
        let (backend, identity, key, root) = fixture();
        let lines = vec![line(1, &key), line(2, &key)];
        segment(&backend, &identity, 1, 2, &lines);
        // The folder of the file is made by the first fetch (an overlay layer's first clone).
        let seen = root.join("layer").join("seen-records");
        assert_eq!(records_with_local(&backend, &identity, PROJECT, &[], Some(&seen)).unwrap().lines.len(), 2);
        assert!(seen.is_file());
        // The records are here and the object has the same time: the next fetch does not read it.
        let object = root.join(format::segment_name(MACHINE, 1, 2));
        let time = std::fs::metadata(&object).unwrap().modified().unwrap();
        std::fs::write(&object, b"not a segment").unwrap();
        std::fs::File::options().write(true).open(&object).unwrap().set_modified(time).unwrap();
        assert_eq!(records_with_local(&backend, &identity, PROJECT, &lines, Some(&seen)).unwrap().lines.len(), 2);
        // A replaced object has another time: it is read and checked again.
        let mut other = Record::untrusted_preview("config");
        other.id = "01a12290-0000-7000-8000-0000000000ff".into();
        other.machine = MACHINE.into();
        other.project = PROJECT.into();
        other.seq = 2;
        let forged = vec![line(1, &key), oplog::sign_line(&other, &key).unwrap()];
        std::fs::remove_file(&object).unwrap();
        segment(&backend, &identity, 1, 2, &forged);
        std::fs::File::options()
            .write(true)
            .open(&object)
            .unwrap()
            .set_modified(time + std::time::Duration::from_secs(10))
            .unwrap();
        let error = records_with_local(&backend, &identity, PROJECT, &lines, Some(&seen)).unwrap_err().to_string();
        assert!(error.contains("conflicting history"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writers_can_forward_overlapping_segments_of_a_third_machine() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=12).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 1, 10, &lines[..10]);
        segment(&backend, &identity, 1, 12, &lines);
        let merged = records(&backend, &identity, PROJECT).unwrap();
        assert_eq!(merged.lines.len(), 12);
        assert_eq!(merged.overlaps, 10);
        assert!(merged.gaps.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn conflicting_history_rejects_the_complete_machine_set() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=3).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 1, 2, &lines[..2]);
        let mut fork = lines;
        let mut record = oplog::parse_line(&fork[1]).unwrap().record;
        record.actor.name = "a different signed record".into();
        fork[1] = oplog::sign_line(&record, &key).unwrap();
        segment(&backend, &identity, 1, 3, &fork);
        assert!(records(&backend, &identity, PROJECT).unwrap_err().to_string().contains("conflicting history"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_gap_does_not_hide_a_contiguous_prefix_and_an_overlap_can_fill_it() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=5).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 1, 2, &lines[..2]);
        segment(&backend, &identity, 4, 5, &lines[3..]);
        let prefix = records(&backend, &identity, PROJECT).unwrap();
        assert_eq!(prefix.lines.len(), 2);
        assert_eq!(prefix.gaps.len(), 1);
        segment(&backend, &identity, 2, 4, &lines[1..4]);
        let complete = records(&backend, &identity, PROJECT).unwrap();
        assert_eq!(complete.lines.len(), 5);
        assert!(complete.gaps.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_changed_machine_key_is_reported_before_local_history_merge() {
        let (backend, identity, key, root) = fixture();
        let local = vec![line(1, &key)];
        let other = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        segment(&backend, &identity, 1, 1, &[line(1, &other)]);
        let error = records_with_local(&backend, &identity, PROJECT, &local, None).unwrap_err().to_string();
        assert!(error.contains("first-record key does not match"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signed_tree_and_file_hash_detect_mutation() {
        let (backend, identity, key, root) = fixture();
        let recipient = keys::recipient_of(&identity).unwrap();
        let content = b"remote state bytes";
        let frame = zst::compress(content, None).unwrap();
        let entry = Entry(
            "100644".into(),
            content.len() as u64,
            hex::encode(ContentAddress::digest(content)),
            vec![format::Link(STATE.into(), "0123abcd".into(), 0, 0, frame.len() as u64, "full".into())],
        );
        let tree =
            Tree { files: BTreeMap::from([("a.txt".into(), entry.clone())]), times: BTreeMap::new(), chunks: BTreeMap::new() };
        let canonical = tree.canonical().unwrap();
        let mut header = Header {
            v: 2,
            state: STATE.into(),
            attempt: "0123abcd".into(),
            tree: "full".into(),
            base: None,
            parts: 1,
            machine: MACHINE.into(),
            btrfs: None,
            sig: String::new(),
        };
        header.sign(&canonical, &key).unwrap();
        let mut object = super::super::canonical::canonical(&serde_json::to_value(&header).unwrap()).unwrap();
        object.push(b'\n');
        object.extend(zst::compress(&canonical, None).unwrap());
        backend.put_new(&format::tree_name(STATE, "0123abcd"), keys::encrypt(&recipient, &object).unwrap()).unwrap();
        backend.put_new(&format::data_name(STATE, "0123abcd", 0), keys::encrypt(&recipient, &frame).unwrap()).unwrap();
        let keys = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
        let mut reader = Reader::new(&backend, &identity, &keys);
        assert_eq!(reader.tree(STATE, "0123abcd").unwrap().2, tree);
        assert_eq!(reader.read(&entry).unwrap(), content);
        let mut wrong = entry;
        wrong.2 = "00".repeat(32);
        assert!(reader.read(&wrong).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fetch_continues_from_an_existing_verified_prefix() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=5).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 3, 5, &lines[2..]);
        let fetched = records_with_local(&backend, &identity, PROJECT, &lines[..2], None).unwrap();
        assert_eq!(fetched.lines.len(), 5);
        assert!(fetched.gaps.is_empty());
        assert_eq!(fetched.overlaps, 0);
        assert_eq!(records(&backend, &identity, PROJECT).unwrap().gaps.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn hydration_stamp_must_bind_the_local_history() {
        for legacy in [false, true] {
            let (backend, identity, key, root) = fixture();
            let original: Vec<String> = (1..=3).map(|seq| line(seq, &key)).collect();
            segment(&backend, &identity, 1, 2, &original[..2]);
            let seen = root.join("metadata/seen-records");
            // Hydration checks a remote machine that has not been imported locally.
            records_with_local(&backend, &identity, PROJECT, &[], Some(&seen)).unwrap();
            if legacy {
                let stamps = std::fs::read_to_string(&seen).unwrap();
                let old: String = stamps
                    .lines()
                    .map(|stamp| {
                        let fields: Vec<_> = stamp.split_whitespace().take(2).collect();
                        format!("{}\n", fields.join(" "))
                    })
                    .collect();
                std::fs::write(&seen, old).unwrap();
            }
            // A different peer supplies a validly signed fork of that machine later.
            let mut fork = original;
            let mut record = oplog::parse_line(&fork[1]).unwrap().record;
            record.actor.name = "a different signed record".into();
            fork[1] = oplog::sign_line(&record, &key).unwrap();
            // Forwarding it adds an overlapping segment; the original object is unchanged.
            segment(&backend, &identity, 1, 3, &fork);
            let uncached = records(&backend, &identity, PROJECT).unwrap_err().to_string();
            assert!(uncached.contains("conflicting history"), "{uncached}");
            let cached = records_with_local(&backend, &identity, PROJECT, &fork, Some(&seen));
            std::fs::remove_dir_all(root).unwrap();
            assert!(cached.is_err(), "cached reader accepted conflicting history rejected by the former push reader: {cached:?}");
        }
    }
}
