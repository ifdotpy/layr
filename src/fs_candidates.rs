//! Native stream-to-candidate normalization, below comparison orchestration.
use crate::compare::{pair_rename, DataEvidence, EntryChange};
use crate::fsutil::Meta;
use crate::stream;
use anyhow::Result;
use layr_fs_core::{RelDir, TreeRead};
use std::collections::{BTreeMap, HashSet};

pub(crate) fn build(parent: &dyn TreeRead, child: &dyn TreeRead, entries: Vec<stream::Entry>) -> Result<Vec<EntryChange>> {
    // Every path the stream touched, in the parent and in the child. `Some(ranges)` when the only
    // entry at the path kept its inode there, so unchanged byte ranges share extents.
    let mut paths: BTreeMap<String, Option<Vec<(u64, crate::compare::RangeEnd)>>> = BTreeMap::new();
    let mut moves: Vec<(String, String, Vec<(u64, crate::compare::RangeEnd)>)> = Vec::new();
    let mut moved_dirs: Vec<(String, String)> = Vec::new();
    let deleted: HashSet<String> = entries.iter().filter(|e| e.path.is_none()).filter_map(|e| e.origin.clone()).collect();
    let touch = |paths: &mut BTreeMap<String, Option<Vec<(u64, crate::compare::RangeEnd)>>>,
                 p: &str,
                 r: Option<Vec<(u64, crate::compare::RangeEnd)>>| match paths.get(p) {
        Some(_) => {
            paths.insert(p.to_string(), None);
        }
        None => {
            paths.insert(p.to_string(), r);
        }
    };
    for e in &entries {
        match (&e.origin, &e.path) {
            (Some(o), Some(p)) if o == p => touch(&mut paths, p, Some(e.ranges.clone())),
            (Some(o), Some(p)) => {
                touch(&mut paths, o, None);
                touch(&mut paths, p, None);
                moves.push((o.clone(), p.clone(), e.ranges.clone()));
                let od = observe(parent, o)?.is_some_and(|m| m.is_dir());
                let nd = observe(child, p)?.is_some_and(|m| m.is_dir());
                if od && nd {
                    moved_dirs.push((o.clone(), p.clone()));
                }
            }
            (Some(o), None) => touch(&mut paths, o, None),
            (None, Some(p)) => {
                touch(&mut paths, p, None);
                if let Some(l) = &e.linked_from {
                    touch(&mut paths, l, None);
                    if deleted.contains(l) {
                        moves.push((l.clone(), p.clone(), e.ranges.clone()));
                    }
                }
            }
            (None, None) => {}
        }
    }
    // Content below a moved folder moved too. One walk per outermost moved folder; a folder
    // moved inside it again maps its own files (the longest moved prefix of a path wins).
    let map: BTreeMap<String, String> = moved_dirs.iter().map(|(o, p)| (p.clone(), o.clone())).collect();
    let roots: Vec<&String> = map.keys().filter(|p| !p.match_indices('/').any(|(i, _)| map.contains_key(&p[..i]))).collect();
    for p in roots {
        let mut found: Vec<(String, String)> = Vec::new();
        layr_fs_common::walk(child, &RelDir::from_bytes(p.as_bytes())?, &mut |rel, _meta| {
            let rel = rel.utf8()?;
            if let Some((top, from)) = rel.rmatch_indices('/').find_map(|(i, _)| map.get(&rel[..i]).map(|o| (i, o))) {
                found.push((format!("{from}/{}", &rel[top + 1..]), rel.to_owned()));
            }
            Ok(true)
        })?;
        for (src, dst) in found {
            if !paths.contains_key(&dst) {
                paths.insert(dst.clone(), None);
                if !paths.contains_key(&src) {
                    paths.insert(src.clone(), None);
                }
                moves.push((src, dst, Vec::new()));
            }
        }
    }
    let mut out: BTreeMap<String, EntryChange> = BTreeMap::new();
    for (p, ranges) in paths {
        let old = observe(parent, &p)?;
        let new = observe(child, &p)?;
        let data = match (ranges, &old, &new) {
            (Some(ranges), Some(old), Some(new)) => {
                DataEvidence::from_ranges(ranges.try_into().map_err(anyhow::Error::msg)?, old.size, new.size)
            }
            _ => DataEvidence::Unknown,
        };
        if let Some(change) = EntryChange::observed(p.clone(), old, new, data) {
            out.insert(p, change);
        }
    }
    // Moves: the source is gone in the child and the target is new in the parent.
    let mut used: HashSet<String> = HashSet::new();
    for (src, dst, ranges) in moves {
        if used.contains(&src) || used.contains(&dst) {
            continue;
        }
        let data = match (out.get(&src).and_then(EntryChange::old), out.get(&dst).and_then(EntryChange::new)) {
            (Some(old), Some(new)) => {
                DataEvidence::from_ranges(ranges.try_into().map_err(anyhow::Error::msg)?, old.size, new.size)
            }
            _ => continue,
        };
        pair_rename(&mut out, &src, &dst, data);
        if !out.get(&dst).is_some_and(|c| c.old_path().is_some()) {
            continue;
        }
        used.insert(src);
        used.insert(dst);
    }
    Ok(out.into_values().collect())
}

fn observe(tree: &dyn TreeRead, path: &str) -> Result<Option<Meta>> {
    let metadata = match tree.metadata(&RelDir::from_bytes(path.as_bytes())?) {
        Ok(metadata) => metadata,
        Err(layr_fs_core::FsError::Io(error))
            if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    Ok(metadata.as_ref().map(layr_fs_common::observation))
}
