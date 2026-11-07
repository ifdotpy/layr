//! Object ids: SHA-256 of a typed header and the content (docs/objects.md, "Object ids").
//!
//! The id names the logical content, not its stored form: a blob keeps its id when the store
//! keeps it whole, as a delta or as chunks. A Secret project keys the hash with a project key
//! (HMAC-SHA256), so equal content gets equal ids only inside that project.

use anyhow::{bail, Result};
use hmac::{Hmac, Mac};
use layr_hashers::{ContentAddress, Digest};
use std::fmt;

/// A 32-byte object id. Text form: 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id(pub [u8; 32]);

impl Id {
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn parse(text: &str) -> Result<Id> {
        if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            bail!("an object id has 64 lowercase hex digits: {text}");
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(text, &mut out)?;
        Ok(Id(out))
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Id({})", &self.hex()[..12])
    }
}

/// What an object holds. The kind is part of the hashed header, so a blob and a tree with equal
/// bytes get different ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Blob,
    Tree,
    Chunk,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Chunk => "chunk",
        }
    }

    pub(crate) fn byte(self) -> u8 {
        match self {
            Kind::Blob => b'b',
            Kind::Tree => b't',
            Kind::Chunk => b'c',
        }
    }

    pub(crate) fn from_byte(byte: u8) -> Result<Kind> {
        Ok(match byte {
            b'b' => Kind::Blob,
            b't' => Kind::Tree,
            b'c' => Kind::Chunk,
            _ => bail!("unknown object kind byte {byte:#x}"),
        })
    }
}

/// How ids are made: a plain hash (Public and Private projects) or a keyed hash (Secret projects).
#[derive(Clone)]
pub enum Keying {
    Plain,
    Keyed([u8; 32]),
}

impl Keying {
    /// A streaming hasher for an object of `kind` with `len` content bytes.
    pub fn hasher(&self, kind: Kind, len: u64) -> Hasher {
        let header = format!("{} {len}\0", kind.name());
        let mut inner = match self {
            Keying::Plain => Inner::Plain(<ContentAddress as Digest>::new()),
            Keying::Keyed(key) => Inner::Keyed(Hmac::<sha2::Sha256>::new_from_slice(key).expect("any key length")),
        };
        inner.update(header.as_bytes());
        Hasher { inner, left: len }
    }

    pub fn id(&self, kind: Kind, bytes: &[u8]) -> Id {
        let mut hasher = self.hasher(kind, bytes.len() as u64);
        hasher.update(bytes);
        hasher.finish().expect("the length is exact")
    }

    /// The name stored in the store configuration.
    pub fn name(&self) -> &'static str {
        match self {
            Keying::Plain => "sha256",
            Keying::Keyed(_) => "hmac-sha256",
        }
    }
}

/// The id key of a Secret project: HKDF-SHA256 (RFC 5869) with the project identity as the input
/// key, the project id as the salt and a fixed label, as `layr_store::chunks::key` makes chunk keys.
pub fn project_key(identity: &str, project: &str) -> [u8; 32] {
    let mut extract = Hmac::<sha2::Sha256>::new_from_slice(project.as_bytes()).expect("any key length");
    extract.update(identity.as_bytes());
    let prk = extract.finalize().into_bytes();
    let mut expand = Hmac::<sha2::Sha256>::new_from_slice(&prk).expect("any key length");
    expand.update(b"layr object id key v1");
    expand.update(&[1]);
    expand.finalize().into_bytes().into()
}

enum Inner {
    Plain(ContentAddress),
    Keyed(Hmac<sha2::Sha256>),
}

impl Inner {
    fn update(&mut self, bytes: &[u8]) {
        match self {
            Inner::Plain(h) => Digest::update(h, bytes),
            Inner::Keyed(h) => h.update(bytes),
        }
    }
}

pub struct Hasher {
    inner: Inner,
    left: u64,
}

impl Hasher {
    pub fn update(&mut self, bytes: &[u8]) {
        self.left = self.left.wrapping_sub(bytes.len() as u64);
        self.inner.update(bytes);
    }

    /// The id; fails when the content length differs from the length in the header (a file that
    /// changed while it was read).
    pub fn finish(self) -> Result<Id> {
        if self.left != 0 {
            bail!("the content length differs from its header (the file changed while it was read)");
        }
        Ok(Id(match self.inner {
            Inner::Plain(h) => h.finalize().into(),
            Inner::Keyed(h) => h.finalize().into_bytes().into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ids_are_fixed() {
        // The id of an empty blob is part of the format: it must never change.
        let empty = Keying::Plain.id(Kind::Blob, b"");
        let mut sha = <ContentAddress as Digest>::new();
        Digest::update(&mut sha, b"blob 0\0");
        assert_eq!(empty.0, <[u8; 32]>::from(sha.finalize()));
        assert_eq!(empty.hex(), "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813");
        assert_eq!(Id::parse(&empty.hex()).unwrap(), empty);
    }

    #[test]
    fn the_kind_and_the_key_change_the_id() {
        let blob = Keying::Plain.id(Kind::Blob, b"x");
        assert_ne!(blob, Keying::Plain.id(Kind::Tree, b"x"));
        assert_ne!(blob, Keying::Plain.id(Kind::Chunk, b"x"));
        let a = Keying::Keyed(project_key("AGE-SECRET-KEY-1A", "project-1"));
        let b = Keying::Keyed(project_key("AGE-SECRET-KEY-1A", "project-2"));
        assert_ne!(a.id(Kind::Blob, b"x"), blob);
        assert_ne!(a.id(Kind::Blob, b"x"), b.id(Kind::Blob, b"x"));
        assert_eq!(a.id(Kind::Blob, b"x"), a.id(Kind::Blob, b"x"));
    }

    #[test]
    fn a_short_read_is_refused() {
        let mut h = Keying::Plain.hasher(Kind::Blob, 5);
        h.update(b"abc");
        assert!(h.finish().is_err());
        for bad in ["", "AB", &"g".repeat(64), &"A".repeat(64)] {
            assert!(Id::parse(bad).is_err());
        }
    }
}
