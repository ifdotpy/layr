//! Parser for `btrfs send --no-data` streams, and a namespace model that turns the command
//! sequence into entries with their path in the parent and in the child.
//!
//! The stream refers to paths in the receiver's tree as it is while the stream is applied, so a
//! path can be a temporary orphan name (`o<ino>-<gen>-<seq>`) or a path below a folder that was
//! renamed earlier in the stream. The model replays renames on a tree of touched entries; an
//! entry that the stream did not create has an origin path in the parent snapshot.

use anyhow::{bail, Result};
use std::collections::HashMap;
use std::io::Read;

const MAGIC: &[u8] = b"btrfs-stream\0";

mod cmd {
    pub const MKFILE: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const MKNOD: u16 = 5;
    pub const MKFIFO: u16 = 6;
    pub const MKSOCK: u16 = 7;
    pub const SYMLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const LINK: u16 = 10;
    pub const UNLINK: u16 = 11;
    pub const RMDIR: u16 = 12;
    pub const WRITE: u16 = 15;
    pub const CLONE: u16 = 16;
    pub const TRUNCATE: u16 = 17;
    pub const CHMOD: u16 = 18;
    pub const UPDATE_EXTENT: u16 = 22;
    pub const FALLOCATE: u16 = 23;
    pub const ENCODED_WRITE: u16 = 25;
}

mod attr {
    pub const SIZE: u16 = 4;
    pub const MODE: u16 = 5;
    pub const PATH: u16 = 15;
    pub const PATH_TO: u16 = 16;
    pub const PATH_LINK: u16 = 17;
    pub const FILE_OFFSET: u16 = 18;
    pub const CLONE_LEN: u16 = 24;
}

#[derive(Debug, Clone, PartialEq)]
pub enum Cmd {
    Create { path: String, kind: Kind },
    Symlink { path: String, target: String },
    Rename { from: String, to: String },
    Link { path: String, target: String },
    Unlink { path: String },
    Rmdir { path: String },
    Extent { path: String, offset: u64, len: u64 },
    Truncate { path: String, size: u64 },
    Chmod { path: String, mode: u32 },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Special,
}

fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}
fn le64(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[..8]);
    u64::from_le_bytes(a)
}

/// The attributes of a command that `decode` needs, as fixed fields.
#[derive(Default)]
struct Attrs<'a> {
    path: Option<&'a [u8]>,
    path_to: Option<&'a [u8]>,
    path_link: Option<&'a [u8]>,
    file_offset: u64,
    size: u64,
    mode: u64,
    clone_len: u64,
}

impl<'a> Attrs<'a> {
    fn scan(body: &'a [u8]) -> Self {
        let num = |v: &[u8]| if v.len() >= 8 { le64(v) } else { 0 };
        let mut out = Attrs::default();
        let mut a = 0;
        while a + 4 <= body.len() {
            let t = le16(&body[a..]);
            let l = le16(&body[a + 2..]) as usize;
            let s = a + 4;
            if s + l > body.len() {
                break;
            }
            let v = &body[s..s + l];
            match t {
                attr::PATH => out.path = Some(v),
                attr::PATH_TO => out.path_to = Some(v),
                attr::PATH_LINK => out.path_link = Some(v),
                attr::FILE_OFFSET => out.file_offset = num(v),
                attr::SIZE => out.size = num(v),
                attr::MODE => out.mode = num(v),
                attr::CLONE_LEN => out.clone_len = num(v),
                _ => {}
            }
            a = s + l;
        }
        out
    }
}

/// Read a send stream command by command and call `f` for each. Unknown commands become
/// `Cmd::Other`. Only one command body is in memory. The stream is read to its end.
pub fn decode(mut r: impl Read, mut f: impl FnMut(Cmd)) -> Result<()> {
    let mut head = [0u8; MAGIC.len() + 4];
    if r.read_exact(&mut head).is_err() || &head[..MAGIC.len()] != MAGIC {
        bail!("not a btrfs send stream");
    }
    let mut header = [0u8; 10];
    let mut body = Vec::new();
    loop {
        let n = read_all(&mut r, &mut header)?;
        if n == 0 {
            return Ok(());
        }
        if n < header.len() {
            bail!("truncated send stream");
        }
        let len = le32(&header) as usize;
        let code = le16(&header[4..]);
        body.resize(len, 0);
        if r.read_exact(&mut body).is_err() {
            bail!("truncated send stream");
        }
        let at = Attrs::scan(&body);
        let path = |v: Option<&[u8]>| -> Result<String> {
            match v {
                Some(v) => Ok(String::from_utf8_lossy(v).into_owned()),
                None => bail!("send command {code} without a path attribute"),
            }
        };
        let c = match code {
            cmd::MKFILE => Cmd::Create { path: path(at.path)?, kind: Kind::File },
            cmd::MKDIR => Cmd::Create { path: path(at.path)?, kind: Kind::Dir },
            cmd::MKNOD | cmd::MKFIFO | cmd::MKSOCK => Cmd::Create { path: path(at.path)?, kind: Kind::Special },
            cmd::SYMLINK => Cmd::Symlink { path: path(at.path)?, target: path(at.path_link)? },
            cmd::RENAME => Cmd::Rename { from: path(at.path)?, to: path(at.path_to)? },
            cmd::LINK => Cmd::Link { path: path(at.path)?, target: path(at.path_link)? },
            cmd::UNLINK => Cmd::Unlink { path: path(at.path)? },
            cmd::RMDIR => Cmd::Rmdir { path: path(at.path)? },
            cmd::UPDATE_EXTENT => Cmd::Extent { path: path(at.path)?, offset: at.file_offset, len: at.size },
            cmd::CLONE => Cmd::Extent { path: path(at.path)?, offset: at.file_offset, len: at.clone_len },
            cmd::WRITE | cmd::ENCODED_WRITE | cmd::FALLOCATE => {
                Cmd::Extent { path: path(at.path)?, offset: at.file_offset, len: u64::MAX }
            }
            cmd::TRUNCATE => Cmd::Truncate { path: path(at.path)?, size: at.size },
            cmd::CHMOD => Cmd::Chmod { path: path(at.path)?, mode: at.mode as u32 },
            _ => Cmd::Other,
        };
        f(c);
    }
}

/// Fill `buf` from `r`; fewer bytes only at the end of the stream.
fn read_all(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
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
    pub ranges: Vec<(u64, u64)>,
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
            Cmd::Extent { path, offset, len } => {
                let id = self.lookup(path);
                let n = &mut self.nodes[id];
                n.touched = true;
                n.entry.content = true;
                n.entry.ranges.push((*offset, *len));
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

    fn command(code: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (t, v) in attrs {
            body.extend_from_slice(&t.to_le_bytes());
            body.extend_from_slice(&(v.len() as u16).to_le_bytes());
            body.extend_from_slice(v);
        }
        let mut out = (body.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend(body);
        out
    }

    #[test]
    fn decode_reads_commands_one_by_one() {
        let mut data = MAGIC.to_vec();
        data.extend_from_slice(&1u32.to_le_bytes());
        data.extend(command(cmd::MKFILE, &[(attr::PATH, b"a")]));
        data.extend(command(cmd::RENAME, &[(attr::PATH, b"a"), (attr::PATH_TO, b"b")]));
        data.extend(command(cmd::UPDATE_EXTENT, &[(attr::PATH, b"b"), (attr::FILE_OFFSET, &4096u64.to_le_bytes()), (attr::SIZE, &8u64.to_le_bytes())]));
        data.extend(command(99, &[]));
        let mut got = Vec::new();
        decode(&data[..], |c| got.push(c)).unwrap();
        assert_eq!(
            got,
            vec![
                Cmd::Create { path: s("a"), kind: Kind::File },
                Cmd::Rename { from: s("a"), to: s("b") },
                Cmd::Extent { path: s("b"), offset: 4096, len: 8 },
                Cmd::Other,
            ]
        );
        assert!(decode(&data[..data.len() - 1], |_| {}).is_err());
        assert!(decode(&b"nope"[..], |_| {}).is_err());
    }

    #[test]
    fn rename_of_folder_maps_children() {
        let cmds = vec![Cmd::Rename { from: s("a"), to: s("b") }, Cmd::Extent { path: s("b/x.txt"), offset: 0, len: 10 }];
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
