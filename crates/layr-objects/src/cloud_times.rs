//! Convert local per-folder times tables to and from the flat Cloud V1 format.
use crate::{times, tree, Id, Store};
use anyhow::{bail, ensure, Context, Result};

const VERSION: u8 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    ns: i64,
    /// Number of descendants, or None for a file or symbolic link.
    below: Option<usize>,
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}
fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}
fn encode(entries: &[Entry]) -> Vec<u8> {
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
fn decode(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut reader = tree::Reader::new(bytes);
    ensure!(reader.byte()? == VERSION, "unknown Cloud times format");
    let count = usize::try_from(reader.varint()?)?;
    ensure!(count > 0 && count <= bytes.len(), "invalid Cloud times count");
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
            ensure!(below == Some(count - 1), "invalid Cloud times root");
        } else {
            ensure!(!open.is_empty(), "Cloud time is outside the root");
        }
        if let Some(n) = below {
            let end = i
                .checked_add(1)
                .and_then(|v| v.checked_add(n))
                .filter(|end| *end <= count)
                .context("Cloud times span is out of bounds")?;
            ensure!(open.last().is_none_or(|parent| end <= *parent), "Cloud times span leaves its parent");
            open.push(end);
        }
        entries.push(Entry { ns: previous, below });
    }
    ensure!(reader.done() && encode(&entries) == bytes, "noncanonical Cloud times");
    Ok(entries)
}

/// Check a flat Cloud V1 times table before it enters the local store.
pub fn validate(bytes: &[u8]) -> Result<()> {
    decode(bytes).map(|_| ())
}

/// Encode a local tree and its per-folder times as one Cloud V1 times table.
pub fn from_local(store: &Store, tree_id: &Id, times_id: &Id) -> Result<Vec<u8>> {
    enum Step {
        Folder(Id, Id),
        File(i64),
        End(usize, Id, Id),
    }
    let mut out = Vec::new();
    let mut stack = vec![Step::Folder(*tree_id, *times_id)];
    let mut active = std::collections::HashSet::new();
    while let Some(step) = stack.pop() {
        match step {
            Step::Folder(tree_id, times_id) => {
                ensure!(active.insert((tree_id, times_id)), "cycle in local times tree");
                let tree = store.get_tree(&tree_id)?;
                let bytes = store.times_bytes(&times_id)?;
                let times = if bytes.first() == Some(&1) {
                    times::Times::decode_legacy_folder_v1(&bytes)?
                } else {
                    times::Times::decode(&bytes)?
                };
                ensure!(tree.entries.len() == times.records.len(), "tree and times counts differ");
                let at = out.len();
                out.push(Entry { ns: times.own, below: Some(0) });
                stack.push(Step::End(at, tree_id, times_id));
                for (entry, time) in tree.entries.into_iter().zip(times.records).rev() {
                    match (entry.node, time) {
                        (tree::Node::Dir { tree }, times::Record::Folder(child)) => stack.push(Step::Folder(tree, child)),
                        (tree::Node::File { .. } | tree::Node::Symlink { .. }, times::Record::Time(ns)) => {
                            stack.push(Step::File(ns))
                        }
                        _ => bail!("tree and times entries differ"),
                    }
                }
            }
            Step::File(ns) => out.push(Entry { ns, below: None }),
            Step::End(at, tree_id, times_id) => {
                out[at].below = Some(out.len() - at - 1);
                active.remove(&(tree_id, times_id));
            }
        }
    }
    Ok(encode(&out))
}

/// Verify a flat Cloud V1 table against a tree and save local per-folder tables.
pub fn to_local(store: &Store, tree_id: &Id, bytes: &[u8]) -> Result<Id> {
    struct Frame {
        own: i64,
        start: usize,
        below: usize,
        entries: std::vec::IntoIter<tree::Entry>,
        times: Vec<times::Record>,
    }
    fn folder(store: &Store, tree_id: &Id, entries: &[Entry], at: &mut usize) -> Result<Frame> {
        let start = *at;
        let current = *entries.get(*at).context("missing folder time")?;
        let below = current.below.context("folder has a file time")?;
        *at += 1;
        let tree = store.get_tree(tree_id)?;
        Ok(Frame {
            own: current.ns,
            start,
            below,
            times: Vec::with_capacity(tree.entries.len()),
            entries: tree.entries.into_iter(),
        })
    }
    let entries = decode(bytes)?;
    let mut at = 0;
    let mut stack = vec![folder(store, tree_id, &entries, &mut at)?];
    loop {
        let frame = stack.last_mut().expect("a root frame is open");
        if let Some(entry) = frame.entries.next() {
            match entry.node {
                tree::Node::Dir { tree } => stack.push(folder(store, &tree, &entries, &mut at)?),
                tree::Node::File { .. } | tree::Node::Symlink { .. } => {
                    let item = *entries.get(at).context("missing file time")?;
                    ensure!(item.below.is_none(), "file has a folder time");
                    at += 1;
                    frame.times.push(times::Record::Time(item.ns));
                }
            }
        } else {
            let frame = stack.pop().expect("a frame is open");
            ensure!(frame.below == at - frame.start - 1, "folder times span differs from tree");
            let id = store.put_times(&times::Times { own: frame.own, records: frame.times })?;
            if let Some(parent) = stack.last_mut() {
                parent.times.push(times::Record::Folder(id));
            } else {
                ensure!(at == entries.len(), "Cloud times contain extra entries");
                return Ok(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{testutil::scratch, Entry as TreeEntry, Keying, Node, Tree};

    #[test]
    fn cloud_times_round_trip_nested_folders_and_reject_wrong_shapes() {
        let dir = scratch("cloud-times");
        let store = Store::open(&dir, Keying::Plain).unwrap();
        let child_tree = store.put_tree(&Tree::default()).unwrap();
        let tree = store
            .put_tree(&Tree {
                entries: vec![
                    TreeEntry { name: b"dir".to_vec(), mode: 0o755, node: Node::Dir { tree: child_tree }, xattrs: vec![] },
                    TreeEntry {
                        name: b"file".to_vec(),
                        mode: 0o644,
                        node: Node::File { size: 0, blob: Id([3; 32]) },
                        xattrs: vec![],
                    },
                ],
                links: vec![],
            })
            .unwrap();
        let child = store.put_times(&times::Times { own: -5, records: vec![] }).unwrap();
        let local = store
            .put_times(&times::Times {
                own: i64::MAX,
                records: vec![times::Record::Folder(child), times::Record::Time(i64::MIN)],
            })
            .unwrap();
        let wire = from_local(&store, &tree, &local).unwrap();
        assert_eq!(
            decode(&wire).unwrap(),
            vec![Entry { ns: i64::MAX, below: Some(2) }, Entry { ns: -5, below: Some(0) }, Entry { ns: i64::MIN, below: None }]
        );
        assert_eq!(to_local(&store, &tree, &wire).unwrap(), local);
        let old_times = store.put_cloud_times(&wire).unwrap();
        assert_eq!(store.times_bytes(&old_times).unwrap(), wire);
        store.set_state("old-state", &crate::StateRef { tree, times: old_times }).unwrap();
        assert_eq!(store.state_ref("old-state").unwrap().unwrap().times, local);
        assert_eq!(store.state_ref("old-state").unwrap().unwrap().times, local);
        let mapping = std::fs::read_to_string(store.root().join("states/old-state")).unwrap();
        assert_eq!(
            mapping,
            format!("{{\"times\":\"{old_times}\",\"tree\":\"{tree}\"}}\n"),
            "a read must not replace the state mapping"
        );
        let mut old_folder_bytes = store.get_times(&local).unwrap().encode();
        old_folder_bytes[0] = 1;
        let old_folder = store.put_times_unchecked_for_test(&old_folder_bytes).unwrap();
        store.set_state("old-folder-state", &crate::StateRef { tree, times: old_folder }).unwrap();
        assert_eq!(store.state_ref("old-folder-state").unwrap().unwrap().times, local);
        let wrong = encode(&[Entry { ns: 0, below: Some(2) }, Entry { ns: 1, below: None }, Entry { ns: 2, below: None }]);
        assert!(to_local(&store, &tree, &wrong).is_err());
        assert!(to_local(&store, &tree, &wire[..wire.len() - 1]).is_err());
        assert!(store.put_cloud_times(&[1, 0]).is_err());
    }
}
