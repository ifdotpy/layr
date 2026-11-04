//! Purpose-specific hashing. Persisted and protocol hashes must never change algorithms.

/// BLAKE3 for disposable local verification and versioned cache keys.
pub use blake3::Hasher as FastCryptographic;
/// SHA-1 as used by Git. This retains the existing sha1 implementation, without collision detection.
pub use sha1::Sha1 as GitCompatible;
/// RustCrypto streaming and one-shot interface for the two compatibility hashers.
pub use sha2::Digest;
/// Stable SHA-256 for persisted content, protocol digests, and HMAC.
pub use sha2::Sha256 as ContentAddress;
/// Rapidhash for transient, non-adversarial in-memory keys only; never use for security or persistence.
pub type FastNonCryptographic = rapidhash::v3::RapidStreamHasherV3<'static>;

/// Fast deterministic map hashing for trusted local keys, never peer-controlled inputs.
pub use rustc_hash::FxHasher as FastMapHasher;
pub type FastBuildHasher = std::hash::BuildHasherDefault<FastMapHasher>;
pub type FastMap<K, V> = std::collections::HashMap<K, V, FastBuildHasher>;
pub type FastSet<K> = std::collections::HashSet<K, FastBuildHasher>;

/// Generic streaming and one-shot interface, statically dispatched and inlined.
pub trait PurposeHasher: Sized {
    type Output: AsRef<[u8]>;
    fn new() -> Self;
    fn update(&mut self, bytes: &[u8]);
    fn finish(self) -> Self::Output;
    #[inline]
    fn hash(bytes: &[u8]) -> Self::Output {
        let mut hasher = Self::new();
        hasher.update(bytes);
        hasher.finish()
    }
}

/// Start a Git blob hash with the Git object header for a known byte length.
pub fn git_blob_hasher(len: u64) -> GitCompatible {
    let mut hash = <GitCompatible as Digest>::new();
    Digest::update(&mut hash, format!("blob {len}\0").as_bytes());
    hash
}

/// Compute the Git blob object ID of a byte slice.
pub fn git_blob_id(bytes: &[u8]) -> [u8; 20] {
    let mut hash = git_blob_hasher(bytes.len() as u64);
    Digest::update(&mut hash, bytes);
    hash.finalize().into()
}

macro_rules! digest_hasher {
    ($ty:ty, $size:expr) => {
        impl PurposeHasher for $ty {
            type Output = [u8; $size];
            #[inline]
            fn new() -> Self {
                <Self as Digest>::new()
            }
            #[inline]
            fn update(&mut self, bytes: &[u8]) {
                Digest::update(self, bytes);
            }
            #[inline]
            fn finish(self) -> Self::Output {
                Digest::finalize(self).into()
            }
        }
    };
}
digest_hasher!(GitCompatible, 20);
digest_hasher!(ContentAddress, 32);
impl PurposeHasher for FastCryptographic {
    type Output = [u8; 32];
    #[inline]
    fn new() -> Self {
        Self::new()
    }
    #[inline]
    fn update(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
    #[inline]
    fn finish(self) -> Self::Output {
        *self.finalize().as_bytes()
    }
    #[inline]
    fn hash(bytes: &[u8]) -> Self::Output {
        *blake3::hash(bytes).as_bytes()
    }
}
impl PurposeHasher for FastNonCryptographic {
    type Output = [u8; 8];
    #[inline]
    fn new() -> Self {
        Self::new(&rapidhash::v3::DEFAULT_RAPID_SECRETS)
    }
    #[inline]
    fn update(&mut self, bytes: &[u8]) {
        self.write(bytes);
    }
    #[inline]
    fn finish(self) -> Self::Output {
        rapidhash::v3::RapidStreamHasherV3::finish(&self).to_le_bytes()
    }
    #[inline]
    fn hash(bytes: &[u8]) -> Self::Output {
        rapidhash::v3::rapidhash_v3(bytes).to_le_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stable_golden_digests() {
        assert_eq!(
            <GitCompatible as PurposeHasher>::hash(b"abc"),
            [
                0xa9, 0x99, 0x3e, 0x36, 0x47, 0x06, 0x81, 0x6a, 0xba, 0x3e, 0x25, 0x71, 0x78, 0x50, 0xc2, 0x6c, 0x9c, 0xd0, 0xd8,
                0x9d
            ]
        );
        assert_eq!(
            <ContentAddress as PurposeHasher>::hash(b"abc"),
            [
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae, 0x22, 0x23, 0xb0, 0x03, 0x61,
                0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61, 0xf2, 0x00, 0x15, 0xad
            ]
        );
    }
    fn streaming<H: PurposeHasher>()
    where
        H::Output: PartialEq + std::fmt::Debug,
    {
        for size in [0, 1, 8, 64, 111, 112, 113, 224, 336, 1024, 4096, 65537] {
            let bytes: Vec<u8> = (0..size).map(|n| (n * 31) as u8).collect();
            let expected = H::hash(&bytes);
            for chunk in [1, 7, 64, 112, 4096, 65536] {
                let mut h = H::new();
                h.update(&[]);
                for part in bytes.chunks(chunk) {
                    h.update(part);
                    h.update(&[]);
                }
                assert_eq!(h.finish(), expected, "size {size}, chunk {chunk}");
            }
            let mut h = H::new();
            let mut at = 0;
            for chunk in [1, 7, 112, 3, 224, 13, 4096].into_iter().cycle() {
                if at == bytes.len() {
                    break;
                }
                let end = (at + chunk).min(bytes.len());
                h.update(&bytes[at..end]);
                h.update(&[]);
                at = end;
            }
            assert_eq!(h.finish(), expected, "irregular size {size}");
        }
    }
    #[test]
    fn git_blob_golden() {
        assert_eq!(
            git_blob_id(b"hello\n"),
            [
                0xce, 0x01, 0x36, 0x25, 0x03, 0x0b, 0xa8, 0xdb, 0xa9, 0x06, 0xf7, 0x56, 0x96, 0x7f, 0x9e, 0x9c, 0xa3, 0x94, 0x46,
                0x4a
            ]
        );
    }
    #[test]
    fn streaming_matches() {
        streaming::<GitCompatible>();
        streaming::<ContentAddress>();
        streaming::<FastCryptographic>();
        streaming::<FastNonCryptographic>();
    }
}
