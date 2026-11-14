//! Parsed state and line identifiers.
#![forbid(unsafe_code)]

use std::fmt;
use uuid::Uuid;

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
        assert_eq!(
            LineId::parse("00112233445566778899AABBCCDDEEFF")?.to_string(),
            "00112233-4455-6677-8899-aabbccddeeff"
        );
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
