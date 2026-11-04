//! Purpose-specific hashing. Persisted and protocol hashes must never change algorithms.

/// BLAKE3 for disposable local verification and versioned cache keys.
pub use blake3::Hasher as FastCryptographic;
/// SHA-1 as used by Git. This retains the existing sha1 implementation, without collision detection.
pub use sha1::Sha1 as GitCompatible;
/// RustCrypto streaming and one-shot interface for the two compatibility hashers.
pub use sha2::Digest;
/// Stable SHA-256 for persisted content, protocol digests, and HMAC.
pub use sha2::Sha256 as ContentAddress;
/// XXH3 for transient, non-adversarial in-memory keys only; never use for security or persistence.
pub use xxhash_rust::xxh3::Xxh3 as FastNonCryptographic;

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
        Self::new()
    }
    #[inline]
    fn update(&mut self, bytes: &[u8]) {
        self.update(bytes);
    }
    #[inline]
    fn finish(self) -> Self::Output {
        self.digest().to_le_bytes()
    }
    #[inline]
    fn hash(bytes: &[u8]) -> Self::Output {
        xxhash_rust::xxh3::xxh3_64(bytes).to_le_bytes()
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
        let mut h = H::new();
        h.update(b"a");
        h.update(b"bc");
        assert_eq!(h.finish(), H::hash(b"abc"));
    }
    #[test]
    fn streaming_matches() {
        streaming::<GitCompatible>();
        streaming::<ContentAddress>();
        streaming::<FastCryptographic>();
        streaming::<FastNonCryptographic>();
    }
}
