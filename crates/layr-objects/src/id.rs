//! Object ids and project epoch key derivation.
use hmac::{Hmac, Mac};
use layr_hashers::ContentAddress;
pub use layr_object_format::id::*;

/// The id key of a Secret overlay in one key epoch: HKDF-SHA256 (RFC 5869) with the epoch's age
/// identity (its text form) as the input key, the epoch id as the salt and the info
/// `layr-objects-id-v1` (docs/objects.md, "Secret overlays"). A new epoch gives new ids.
pub fn epoch_key(identity: &str, epoch: &str) -> [u8; 32] {
    let mut extract = Hmac::<ContentAddress>::new_from_slice(epoch.as_bytes()).expect("any key length");
    extract.update(identity.as_bytes());
    let prk = extract.finalize().into_bytes();
    let mut expand = Hmac::<ContentAddress>::new_from_slice(&prk).expect("any key length");
    expand.update(b"layr-objects-id-v1");
    expand.update(&[1]);
    expand.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn the_kind_and_the_key_change_the_id() {
        let blob = Keying::Plain.id(Kind::Blob, b"x");
        assert_ne!(blob, Keying::Plain.id(Kind::Tree, b"x"));
        assert_ne!(blob, Keying::Plain.id(Kind::Chunk, b"x"));
        let a = Keying::Keyed(epoch_key("AGE-SECRET-KEY-1A", "project-1"));
        let b = Keying::Keyed(epoch_key("AGE-SECRET-KEY-1A", "project-2"));
        assert_ne!(a.id(Kind::Blob, b"x"), blob);
        assert_ne!(a.id(Kind::Blob, b"x"), b.id(Kind::Blob, b"x"));
        assert_eq!(a.id(Kind::Blob, b"x"), a.id(Kind::Blob, b"x"));
    }
}
