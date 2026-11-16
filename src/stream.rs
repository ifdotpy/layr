//! IO adapter and namespace replay for local `btrfs send --no-data` observations.
use crate::compare::RangeEnd;
use anyhow::{bail, Result};
pub use layr_stream_codec::Kind;
use layr_stream_codec::{CommandHeader, DecodeLimits, MetadataCommand as Cmd, Version};
use std::{collections::HashMap, io::Read};

/// Read only bounded command bodies. This local metadata policy never authorizes receive.
fn decode(mut r: impl Read, mut f: impl FnMut(Cmd)) -> Result<()> {
    let limits = DecodeLimits::default();
    let mut head = [0; layr_stream_codec::STREAM_HEADER_LEN];
    r.read_exact(&mut head)?;
    let version = Version::decode(&head)?;
    let mut header = [0; layr_stream_codec::COMMAND_HEADER_LEN];
    let mut body = Vec::new();
    loop {
        let mut n = 0;
        while n < header.len() {
            match r.read(&mut header[n..]) {
                Ok(0) => break,
                Ok(k) => n += k,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        if n == 0 {
            return Ok(());
        }
        if n != header.len() {
            bail!("truncated send stream");
        }
        let frame = CommandHeader::decode(&header, limits)?;
        body.resize(frame.body_len(), 0);
        r.read_exact(&mut body)?;
        f(Cmd::decode(frame, &body, version, limits)?);
    }
}

/// One entry touched by the stream.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    /// Path in the parent snapshot, if the entry existed there.
    pub origin: Option<String>,
    /// Path in the child snapshot, if the entry exists there.
    pub path: Option<String>,
    /// Set when the stream created the entry (a new inode or a new name).
    pub created: Option<Kind>,
    /// For a new name made by `link`: the origin path of the inode it links to.
    pub linked_from: Option<String>,
    pub content: bool,
    pub mode: Option<u32>,
    pub ranges: Vec<(u64, RangeEnd)>,
    pub truncated: Option<u64>,
}

#[derive(Default)]
struct Node {
    entry: Entry,
    /// Children known to the model. `None` marks a name known to be absent.
    children: HashMap<String, Option<usize>>,
    touched: bool,
}

struct Model {
    nodes: Vec<Node>,
}

impl Model {
    fn new() -> Self {
        let mut root = Node::default();
        root.entry.origin = Some(String::new());
        Model { nodes: vec![root] }
    }

    fn join(base: &str, name: &str) -> String {
        if base.is_empty() {
            name.to_string()
        } else {
            format!("{base}/{name}")
        }
    }

    /// Find the node for `path`, creating implicit nodes for entries of the parent snapshot.
    fn lookup(&mut self, path: &str) -> usize {
        let mut cur = 0;
        for comp in path.split('/').filter(|c| !c.is_empty()) {
            let next = match self.nodes[cur].children.get(comp) {
                Some(Some(n)) => *n,
                _ => {
                    let origin = if self.nodes[cur].entry.created.is_some() {
                        None
                    } else {
                        self.nodes[cur].entry.origin.as_ref().map(|o| Self::join(o, comp))
                    };
                    let id = self.nodes.len();
                    let mut n = Node::default();
                    n.entry.origin = origin;
                    self.nodes.push(n);
                    self.nodes[cur].children.insert(comp.to_string(), Some(id));
                    id
                }
            };
            cur = next;
        }
        cur
    }

    fn split(path: &str) -> (&str, &str) {
        match path.rfind('/') {
            Some(i) => (&path[..i], &path[i + 1..]),
            None => ("", path),
        }
    }

    fn detach(&mut self, path: &str) -> usize {
        let id = self.lookup(path);
        let (parent, name) = Self::split(path);
        let p = self.lookup(parent);
        self.nodes[p].children.insert(name.to_string(), None);
        id
    }

    fn attach(&mut self, path: &str, id: usize) {
        let (parent, name) = Self::split(path);
        let p = self.lookup(parent);
        if let Some(Some(old)) = self.nodes[p].children.get(name).cloned() {
            if old != id {
                self.nodes[old].touched = true;
            }
        }
        self.nodes[p].children.insert(name.to_string(), Some(id));
    }

    fn create(&mut self, path: &str, kind: Kind) -> usize {
        let id = self.nodes.len();
        let mut n = Node::default();
        n.entry.created = Some(kind);
        n.touched = true;
        self.nodes.push(n);
        self.attach(path, id);
        id
    }

    fn apply(&mut self, c: &Cmd) {
        match c {
            Cmd::Create { path, kind } => {
                self.create(path, *kind);
            }
            Cmd::Symlink { path, .. } => {
                self.create(path, Kind::Symlink);
            }
            Cmd::Rename { from, to } => {
                let id = self.detach(from);
                self.nodes[id].touched = true;
                self.attach(to, id);
            }
            Cmd::Link { path, target } => {
                let t = self.lookup(target);
                let linked = self.nodes[t].entry.origin.clone();
                let id = self.create(path, Kind::File);
                self.nodes[id].entry.linked_from = linked;
            }
            Cmd::Unlink { path } | Cmd::Rmdir { path } => {
                let id = self.detach(path);
                self.nodes[id].touched = true;
            }
            Cmd::Extent { path, offset, end } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.content = true;
                let end = match end {
                    layr_stream_codec::RangeEnd::Offset(end) => RangeEnd::Offset(*end),
                    layr_stream_codec::RangeEnd::Eof => RangeEnd::Eof,
                };
                if end > RangeEnd::Offset(*offset) {
                    n.entry.ranges.push((*offset, end));
                }
            }
            Cmd::Truncate { path, size } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.content = true;
                n.entry.truncated = Some(*size);
            }
            Cmd::Chmod { path, mode } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.mode = Some(*mode);
            }
            Cmd::Metadata { path } => {
                let id = self.lookup(path);
                self.nodes[id].touched = true;
            }
            Cmd::Other => {}
        }
    }

    fn finish(mut self) -> Vec<Entry> {
        // Final paths: walk the tree from the root.
        let mut stack = vec![(0usize, String::new())];
        while let Some((id, path)) = stack.pop() {
            self.nodes[id].entry.path = Some(path.clone());
            let kids: Vec<(String, usize)> =
                self.nodes[id].children.iter().filter_map(|(k, v)| v.map(|v| (k.clone(), v))).collect();
            for (name, kid) in kids {
                stack.push((kid, Self::join(&path, &name)));
            }
        }
        self.nodes
            .into_iter()
            .skip(1)
            .filter(|n| n.touched || n.entry.origin.as_deref() != n.entry.path.as_deref())
            .map(|n| n.entry)
            .filter(|e| e.origin.is_some() || e.path.is_some())
            .collect()
    }
}

/// Read a send stream into touched entries, command by command.
pub fn entries_from(r: impl Read) -> Result<Vec<Entry>> {
    let mut m = Model::new();
    decode(r, |c| m.apply(&c))?;
    Ok(m.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(x: &str) -> String {
        x.to_string()
    }

    fn entries(cmds: &[Cmd]) -> Vec<Entry> {
        let mut m = Model::new();
        for c in cmds {
            m.apply(c);
        }
        m.finish()
    }

    #[test]
    fn decoding_keeps_metadata_paths_through_renames_and_rejects_large_frames() {
        let mut data = layr_stream_codec::MAGIC.to_vec();
        data.extend(1u32.to_le_bytes());
        for (code, attrs) in [
            (13, vec![(15, b"old/file".as_slice())]),
            (9, vec![(15, b"old".as_slice()), (16, b"new".as_slice())]),
            (14, vec![(15, b"new/other".as_slice())]),
            (20, vec![(15, b"new/time".as_slice())]),
        ] {
            data.extend(layr_stream_codec::encode_command(code, &attrs, DecodeLimits::default()).unwrap());
        }
        let got = entries_from(data.as_slice()).unwrap();
        for (old, new) in [("old/file", "new/file"), ("old/other", "new/other"), ("old/time", "new/time")] {
            assert!(got.iter().any(|e| e.origin.as_deref() == Some(old) && e.path.as_deref() == Some(new)));
        }
        data.extend(u32::MAX.to_le_bytes());
        data.extend([0; 6]);
        assert!(entries_from(data.as_slice())
            .unwrap_err()
            .downcast_ref::<layr_stream_codec::DecodeError>()
            .is_some_and(|e| *e == layr_stream_codec::DecodeError::Limit));
        assert!(entries_from(&data[..data.len() - 1]).is_err());
    }

    #[test]
    fn rename_of_folder_maps_children() {
        let cmds = vec![
            Cmd::Rename { from: s("a"), to: s("b") },
            Cmd::Extent { path: s("b/x.txt"), offset: 0, end: layr_stream_codec::RangeEnd::Offset(10) },
        ];
        let e = entries(&cmds);
        let x = e.iter().find(|e| e.path.as_deref() == Some("b/x.txt")).unwrap();
        assert_eq!(x.origin.as_deref(), Some("a/x.txt"));
        let d = e.iter().find(|e| e.path.as_deref() == Some("b")).unwrap();
        assert_eq!(d.origin.as_deref(), Some("a"));
    }

    #[test]
    fn orphan_delete_and_new_file() {
        let cmds = vec![
            Cmd::Rename { from: s("d"), to: s("o259-7-0") },
            Cmd::Unlink { path: s("o259-7-0/f") },
            Cmd::Rmdir { path: s("o259-7-0") },
            Cmd::Create { path: s("o260-8-0"), kind: Kind::File },
            Cmd::Rename { from: s("o260-8-0"), to: s("new.txt") },
        ];
        let e = entries(&cmds);
        assert!(e.iter().any(|e| e.origin.as_deref() == Some("d/f") && e.path.is_none()));
        assert!(e.iter().any(|e| e.origin.as_deref() == Some("d") && e.path.is_none()));
        assert!(e.iter().any(|e| e.origin.is_none() && e.path.as_deref() == Some("new.txt")));
    }

    #[test]
    fn link_then_unlink_is_a_rename() {
        let cmds = vec![Cmd::Link { path: s("new"), target: s("old") }, Cmd::Unlink { path: s("old") }];
        let e = entries(&cmds);
        let n = e.iter().find(|e| e.path.as_deref() == Some("new")).unwrap();
        assert_eq!(n.linked_from.as_deref(), Some("old"));
        assert!(e.iter().any(|e| e.origin.as_deref() == Some("old") && e.path.is_none()));
    }
}
