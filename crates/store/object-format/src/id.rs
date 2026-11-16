//! Object ids: SHA-256 of a typed header and the content (docs/objects.md, "Object ids").
//!
//! The id names the logical content, not its stored form: a blob keeps its id when the store
//! keeps it whole, as a delta or as chunks. A Secret project keys the hash with a project key
//! (HMAC-SHA256), so equal content gets equal ids only inside that project.

use crate::{Error, Result};
use hmac::{Hmac, Mac};
use layr_hashers::{ContentAddress, Digest};

pub use layr_types::ObjectId as Id;

/// What an object holds. The kind is part of the hashed header, so a blob and a tree with equal
/// bytes get different ids.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    Blob,
    Tree,
    Chunk,
    /// Bytes whose structure the id does not describe: a pack, a Git pack, a stored (encoded or
    /// encrypted) form. Anyone can check the id of an opaque object from its bytes alone.
    Opaque,
    /// The times table of a state (`crate::times`): referenced by the state, not by a tree.
    Times,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Blob => "blob",
            Kind::Tree => "tree",
            Kind::Chunk => "chunk",
            Kind::Opaque => "opaque",
            Kind::Times => "times",
        }
    }

    pub fn byte(self) -> u8 {
        match self {
            Kind::Blob => b'b',
            Kind::Tree => b't',
            Kind::Chunk => b'c',
            Kind::Opaque => b'o',
            Kind::Times => b'm',
        }
    }

    pub fn from_byte(byte: u8) -> Result<Kind> {
        Ok(match byte {
            b'b' => Kind::Blob,
            b't' => Kind::Tree,
            b'c' => Kind::Chunk,
            b'o' => Kind::Opaque,
            b'm' => Kind::Times,
            _ => return Err(Error::UnknownKind(byte)),
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
    /// Hash a supplied reader to EOF with caller-owned initialized scratch memory.
    /// Read errors (including interruption) propagate; a changed length is refused.
    pub fn hash_reader(&self, kind: Kind, len: u64, reader: &mut impl std::io::Read, scratch: &mut [u8]) -> Result<Id> {
        if scratch.is_empty() {
            return Err(Error::EmptyScratch);
        }
        let mut hasher = self.hasher(kind, len);
        loop {
            let n = reader.read(scratch).map_err(Error::Read)?;
            if n == 0 {
                break;
            }
            hasher.update(&scratch[..n]);
        }
        hasher.finish()
    }

    /// A streaming hasher for an object of `kind` with `len` content bytes.
    pub fn hasher(&self, kind: Kind, len: u64) -> Hasher {
        let header = format!("{} {len}\0", kind.name());
        let mut inner = match self {
            Keying::Plain => Inner::Plain(<ContentAddress as Digest>::new()),
            // HMAC accepts keys of every length, including this fixed 32-byte key.
            Keying::Keyed(key) => Inner::Keyed(Hmac::<ContentAddress>::new_from_slice(key).expect("any key length")),
        };
        inner.update(header.as_bytes());
        Hasher { inner, left: len }
    }

    pub fn id(&self, kind: Kind, bytes: &[u8]) -> Id {
        let mut hasher = self.hasher(kind, bytes.len() as u64);
        hasher.update(bytes);
        // The header length and the only update both come from the same slice.
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

enum Inner {
    Plain(ContentAddress),
    Keyed(Hmac<ContentAddress>),
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
            return Err(Error::LengthMismatch);
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
    fn supplied_reader_matches_slice_hashes_and_propagates_errors() -> Result<()> {
        use std::io::{self, Read};
        struct Short<'a>(&'a [u8]);
        impl Read for Short<'_> {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let n = out.len().min(3);
                self.0.read(&mut out[..n])
            }
        }
        struct Failed(io::ErrorKind);
        impl Read for Failed {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(self.0.into())
            }
        }
        let mut scratch = [0; 17];
        for keying in [Keying::Plain, Keying::Keyed([7; 32])] {
            for size in [0, 1, 4096, 1 << 20] {
                let bytes = vec![42; size];
                assert_eq!(
                    keying.hash_reader(Kind::Blob, size as u64, &mut bytes.as_slice(), &mut scratch)?,
                    keying.id(Kind::Blob, &bytes),
                );
            }
            let bytes = b"short reads preserve framing";
            assert_eq!(
                keying.hash_reader(Kind::Blob, bytes.len() as u64, &mut Short(bytes), &mut scratch)?,
                keying.id(Kind::Blob, bytes)
            );
            for len in [0, bytes.len() as u64 - 1, bytes.len() as u64 + 1] {
                assert!(matches!(
                    keying.hash_reader(Kind::Blob, len, &mut bytes.as_slice(), &mut scratch),
                    Err(Error::LengthMismatch)
                ));
            }
            for kind in [io::ErrorKind::Interrupted, io::ErrorKind::PermissionDenied] {
                assert!(
                    matches!(keying.hash_reader(Kind::Blob, 0, &mut Failed(kind), &mut scratch), Err(Error::Read(error)) if error.kind() == kind)
                );
            }
            assert!(matches!(keying.hash_reader(Kind::Blob, 0, &mut &b""[..], &mut []), Err(Error::EmptyScratch)));
        }
        // HMAC-SHA256 of the exact persisted header followed by the content.
        assert_eq!(
            Keying::Keyed([7; 32]).id(Kind::Blob, b"hello\n").hex(),
            "1aefb4667ea4e6663cc268f747b875c565742555c4fd33eb6b477c483beb9cfb"
        );
        Ok(())
    }

    #[test]
    fn plain_ids_are_fixed() {
        // The id of an empty blob is part of the format: it must never change.
        let empty = Keying::Plain.id(Kind::Blob, b"");
        let mut sha = <ContentAddress as Digest>::new();
        Digest::update(&mut sha, b"blob 0\0");
        assert_eq!(empty.0, <[u8; 32]>::from(sha.finalize()));
        assert_eq!(empty.hex(), "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813");
        assert_eq!(Id::parse(&empty.hex()).unwrap(), empty);
        assert_eq!(format!("{empty:?}"), "Id(473a0f4c3be8)");
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
