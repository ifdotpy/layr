//! Public epoch descriptors contain audience hashes, never reader identities.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Private,
    Service,
    Public,
}
impl Mode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "private" => Ok(Self::Private),
            "service" => Ok(Self::Service),
            "public" => Ok(Self::Public),
            _ => bail!("access mode must be private, service or public"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Service => "service",
            Self::Public => "public",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Key {
    pub project: String,
    pub parent: String,
    pub parent_recipient: String,
    pub epoch: String,
    pub recipient: String,
    pub mode: Mode,
    pub readers_hash: String,
}
impl Key {
    pub fn validate(&self) -> Result<()> {
        if !crate::format::is_uuid(&self.project)
            || !crate::format::is_uuid(&self.epoch)
            || (self.parent != "root" && !crate::format::is_uuid(&self.parent))
            || !crate::keys::valid_recipient(&self.recipient)
            || !crate::keys::valid_recipient(&self.parent_recipient)
            || self.recipient == self.parent_recipient
            || self.readers_hash.len() != 64
            || !self.readers_hash.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            bail!("invalid project epoch descriptor");
        }
        Ok(())
    }
    pub fn prefix(&self) -> String {
        format!("project/epochs/{}", self.epoch)
    }
    pub fn object(&self, record: &str) -> String {
        format!("keys/{}-{record}.json", self.epoch)
    }
}
pub fn readers_hash(readers: &BTreeMap<String, String>) -> Result<String> {
    Ok(hex::encode(Sha256::digest(crate::canonical::canonical(&serde_json::to_value(readers)?)?)))
}
