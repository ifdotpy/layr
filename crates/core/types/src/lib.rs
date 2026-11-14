//! Parsed state, line, and object identifiers.
#![forbid(unsafe_code)]

use std::fmt;
use uuid::Uuid;

macro_rules! record_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
        pub struct $name(Uuid);
        impl $name {
            pub fn parse(text: &str) -> Result<Self, uuid::Error> {
                Uuid::parse_str(text).map(Self)
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}
record_id!(RecordId);
record_id!(ProjectId);
record_id!(MachineId);

/// An exact line UUID, distinct from a line name or state identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineId(Uuid);

impl LineId {
    pub fn parse(text: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(text).map(Self)
    }
}

impl fmt::Display for LineId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A full state UUID, parsed before taking its display prefix.
pub struct StateId(Uuid);

impl StateId {
    pub fn parse(text: &str) -> Result<Self, uuid::Error> {
        Uuid::parse_str(text).map(Self)
    }

    /// The first seven digits of the UUID's permuted display form.
    #[must_use]
    pub fn short(&self) -> StatePrefix {
        let b = self.0.as_bytes();
        let mut prefix = hex::encode(&b[10..14]);
        prefix.truncate(7);
        StatePrefix(prefix)
    }
}

/// A display prefix, distinct from a full state identifier.
pub struct StatePrefix(String);

impl fmt::Display for StatePrefix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{LineId, StateId};

    #[test]
    fn line_id_requires_a_full_uuid_and_canonicalizes_it() -> Result<(), uuid::Error> {
        assert!(LineId::parse("topic").is_err());
        assert!(LineId::parse("0011223").is_err());
        assert_eq!(LineId::parse("00112233445566778899AABBCCDDEEFF")?.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
        Ok(())
    }

    #[test]
    fn short_accepts_historical_uuid_forms() -> Result<(), uuid::Error> {
        for s in [
            "00112233-4455-6677-8899-aabbccddeeff",
            "00112233445566778899AABBCCDDEEFF",
            "{00112233-4455-6677-8899-aabbccddeeff}",
            "URN:UUID:00112233-4455-6677-8899-aabbccddeeff",
        ] {
            assert_eq!(StateId::parse(s)?.short().to_string(), "aabbccd");
        }
        assert!(StateId::parse("not a uuid").is_err());
        Ok(())
    }
}

/// A 32-byte object id. Text form: 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn parse(text: &str) -> Result<ObjectId, ParseObjectIdError> {
        if text.len() != 64 || !text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return Err(ParseObjectIdError(text.to_owned()));
        }
        let mut out = [0u8; 32];
        // The check above accepts exactly 32 pairs of hex digits.
        hex::decode_to_slice(text, &mut out).map_err(|_| ParseObjectIdError(text.to_owned()))?;
        Ok(ObjectId(out))
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Id({})", &self.hex()[..12])
    }
}

/// An object identifier outside the canonical lowercase hexadecimal form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseObjectIdError(String);

impl fmt::Display for ParseObjectIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "an object id has 64 lowercase hex digits: {}", self.0)
    }
}
impl std::error::Error for ParseObjectIdError {}
