//! Parsed state identifiers.
#![forbid(unsafe_code)]

use std::fmt;
use uuid::Uuid;

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
    use super::StateId;

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
