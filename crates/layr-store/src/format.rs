//! Store v2 trees. Record segments retain the exact signed JSONL bytes.

use super::canonical::canonical;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Public store metadata. In a Secret store the recipient identifies the vault encryption key; Private and
/// Public stores keep plain objects and name [`crate::keys::PLAIN`] as the recipient.
#[derive(Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub format: u32,
    pub project: String,
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Mode::is_secret")]
    pub mode: Mode,
}

/// The project mode (docs/cloud-rfc.md 7.0). Secret: end-to-end encrypted, the server holds ciphertext only.
/// Private and Public: the server reads the code and controls access; objects and records are plain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Secret,
    Private,
    Public,
}

impl Mode {
    pub fn is_secret(&self) -> bool {
        *self == Mode::Secret
    }

    pub fn parse(text: &str) -> Result<Mode> {
        match text {
            "secret" => Ok(Mode::Secret),
            "private" => Ok(Mode::Private),
            "public" => Ok(Mode::Public),
            _ => bail!("unknown project mode {text}; use secret, private or public"),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Mode::Secret => "secret",
            Mode::Private => "private",
            Mode::Public => "public",
        }
    }
}

impl Descriptor {
    /// A descriptor whose recipient matches its mode: an age recipient for Secret, [`crate::keys::PLAIN`] otherwise.
    pub fn check_mode(&self) -> Result<()> {
        let plain = self.recipient == crate::keys::PLAIN;
        if self.mode.is_secret() == plain {
            bail!("store mode {} does not match its recipient", self.mode.name());
        }
        Ok(())
    }
}

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).map(|u| u.hyphenated().to_string() == s).unwrap_or(false)
}

/// One immutable encrypted release file, without any user-controlled path components.
/// A public release file at the root of a store (`release attach --public-files`): `public/release/<vX>/<name>`, the
/// gateway serves it without sign-in. The version is `v<major>.<minor>.<patch>[-<pre>]`; the name has letters, digits,
/// `.`, `_` and `-`, no leading dot, at most 128 bytes.
pub fn public_release_file(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("public/release/") else { return false };
    let Some((version, name)) = rest.split_once('/') else { return false };
    let numbers = version.strip_prefix('v').map(|v| v.split_once('-').map_or(v, |(core, _)| core));
    let version_ok = numbers.is_some_and(|core| {
        let parts: Vec<&str> = core.split('.').collect();
        parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    }) && version.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'));
    let name_ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    version_ok && name_ok
}

pub fn release_file(key: &str) -> bool {
    key.strip_prefix("files/").and_then(|s| s.strip_suffix(".age")).is_some_and(is_uuid)
}

pub const RELEASE_FILE_LIMIT: u64 = 512 << 20;
pub const RELEASE_OBJECT_LIMIT: u64 = RELEASE_FILE_LIMIT + (RELEASE_FILE_LIMIT / 65536 + 1) * 16 + 4096;

pub const PART_LIMIT: usize = 16 << 20;
pub const FULL_TREE_EVERY: usize = 32;
pub const MAX_FILE_DELTAS: usize = 16;
pub const DELTA_LIMIT: usize = 32 << 20;
pub const MAX_PARTS: u32 = 10_000;
const DOMAIN: &[u8] = b"layr/store/v2/tree\0";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub v: u32,
    pub state: String,
    pub attempt: String,
    pub tree: String,
    pub base: Option<(String, String)>,
    pub parts: u32,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs: Option<BtrfsIdentity>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sig: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtrfsIdentity {
    pub uuid: String,
    pub transid: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link(pub String, pub String, pub u32, pub u64, pub u64, pub String);

impl Link {
    pub fn object(&self) -> String {
        data_name(&self.0, &self.1, self.2)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry(pub String, pub u64, pub String, pub Vec<Link>);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tree {
    pub files: BTreeMap<String, Entry>,
    /// The modification time of each file, in nanoseconds since the epoch. Build tools compare them
    /// (cargo rebuilds a unit whose dependency is newer), so a restored cache keeps them. Empty in the
    /// trees of layr 0.25 and older, which restore files with the time of the restore.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub times: BTreeMap<String, u64>,
    /// The content-defined chunks of a file of a cache layer, by chunk id (`chunks::id`), in
    /// order. Such a file has no frame links: its chunks live in pack objects of the layer
    /// (`chunks::Pack`). Empty in trees that only have frames.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub chunks: BTreeMap<String, Vec<String>>,
}

impl Tree {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        use super::canonical::string;
        let mut out = String::from("{");
        // Canonical JSON sorts keys: "chunks" < "files" < "times".
        if !self.chunks.is_empty() {
            out.push_str("\"chunks\":{");
            for (number, (path, ids)) in self.chunks.iter().enumerate() {
                if number != 0 {
                    out.push(',');
                }
                string(path, &mut out);
                out.push_str(":[");
                for (number, id) in ids.iter().enumerate() {
                    if number != 0 {
                        out.push(',');
                    }
                    string(id, &mut out);
                }
                out.push(']');
            }
            out.push_str("},");
        }
        out.push_str("\"files\":{");
        for (number, (path, entry)) in self.files.iter().enumerate() {
            if number != 0 {
                out.push(',');
            }
            string(path, &mut out);
            out.push_str(":[");
            string(&entry.0, &mut out);
            out.push(',');
            out.push_str(&entry.1.to_string());
            out.push(',');
            string(&entry.2, &mut out);
            out.push_str(",[");
            for (number, link) in entry.3.iter().enumerate() {
                if number != 0 {
                    out.push(',');
                }
                out.push('[');
                string(&link.0, &mut out);
                out.push(',');
                string(&link.1, &mut out);
                for value in [link.2 as u64, link.3, link.4] {
                    out.push(',');
                    out.push_str(&value.to_string());
                }
                out.push(',');
                string(&link.5, &mut out);
                out.push(']');
            }
            out.push_str("]]");
        }
        out.push('}');
        if !self.times.is_empty() {
            out.push_str(",\"times\":{");
            for (number, (path, time)) in self.times.iter().enumerate() {
                if number != 0 {
                    out.push(',');
                }
                string(path, &mut out);
                out.push(':');
                out.push_str(&time.to_string());
            }
            out.push('}');
        }
        out.push('}');
        Ok(out.into_bytes())
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tree: Self = serde_json::from_slice(bytes)?;
        if tree.canonical()? != bytes {
            bail!("noncanonical tree");
        }
        if let Some(path) = tree.times.keys().find(|p| !tree.files.contains_key(*p)) {
            bail!("tree time of a missing file {path:?}");
        }
        for (path, ids) in &tree.chunks {
            let entry = tree.files.get(path).with_context(|| format!("tree chunks of a missing file {path:?}"))?;
            if ids.is_empty() || ids.len() as u64 > entry.1 / crate::chunks::MIN as u64 + 1 {
                bail!("invalid chunk count");
            }
            if !ids.iter().all(|id| crate::chunks::valid_id(id)) {
                bail!("invalid chunk id");
            }
        }
        for (path, entry) in &tree.files {
            if !safe_path(path) {
                bail!("unsafe tree path {path:?}");
            }
            if !matches!(entry.0.as_str(), "100644" | "100755" | "120000") {
                bail!("invalid file mode");
            }
            if entry.2.len() != 64 || hex::decode(&entry.2).is_err() {
                bail!("invalid SHA-256");
            }
            // A file has frames or chunks, never both.
            if tree.chunks.contains_key(path) {
                if !entry.3.is_empty() {
                    bail!("a chunked file has frame links");
                }
                continue;
            }
            if entry.3.is_empty() || entry.3.len() > MAX_FILE_DELTAS + 1 {
                bail!("invalid file chain length");
            }
            for (i, link) in entry.3.iter().enumerate() {
                if !is_uuid(&link.0) || !valid_attempt(&link.1) || link.2 >= MAX_PARTS || link.4 == 0 {
                    bail!("invalid frame address");
                }
                let expected = if i + 1 == entry.3.len() { "full" } else { "delta" };
                if link.5 != expected {
                    bail!("file chain must end with one full frame");
                }
                link.3.checked_add(link.4).context("frame range overflow")?;
            }
            // No file can be the parent folder of another file, including a symlink.
            let mut parent = std::path::Path::new(path).parent();
            while let Some(p) = parent {
                if tree.files.contains_key(p.to_str().context("invalid tree path")?) {
                    bail!("tree path has a file parent: {path}");
                }
                parent = p.parent();
            }
        }
        Ok(tree)
    }
}

pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && !path.split('/').any(|s| s.is_empty() || s == "." || s == "..")
        && crate::paths::check_rel(path).is_ok()
}

pub fn valid_attempt(attempt: &str) -> bool {
    attempt.len() == 8 && attempt.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn tree_name(state: &str, attempt: &str) -> String {
    format!("states/{state}/{attempt}.tree.age")
}
pub fn data_name(state: &str, attempt: &str, part: u32) -> String {
    format!("states/{state}/{attempt}.{part:04}.data.age")
}

pub fn parse_tree_name(key: &str) -> Option<(String, String)> {
    let (state, rest) = key.strip_prefix("states/")?.split_once('/')?;
    let attempt = rest.strip_suffix(".tree.age")?;
    (is_uuid(state) && valid_attempt(attempt)).then(|| (state.to_string(), attempt.to_string()))
}

pub fn parse_data_name(key: &str) -> Option<(String, String, u32)> {
    let (state, rest) = key.strip_prefix("states/")?.split_once('/')?;
    let (attempt, part) = rest.strip_suffix(".data.age")?.split_once('.')?;
    if !is_uuid(state) || !valid_attempt(attempt) || part.len() != 4 || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((state.to_string(), attempt.to_string(), part.parse().ok()?))
}

pub fn segment_name(machine: &str, first: u64, last: u64) -> String {
    format!("records/{machine}/{first:020}-{last:020}.age")
}

pub fn parse_segment_name(key: &str) -> Option<(String, u64, u64)> {
    let (machine, range) = key.strip_prefix("records/")?.split_once('/')?;
    let (first, last) = range.strip_suffix(".age")?.split_once('-')?;
    if !is_uuid(machine)
        || first.len() != 20
        || last.len() != 20
        || !first.bytes().all(|b| b.is_ascii_digit())
        || !last.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let (first, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
    (first > 0 && first <= last).then(|| (machine.to_string(), first, last))
}

fn signed_bytes(header: &Header, tree: &[u8]) -> Result<Vec<u8>> {
    let mut unsigned = header.clone();
    unsigned.sig.clear();
    let mut bytes = DOMAIN.to_vec();
    bytes.extend(canonical(&serde_json::to_value(unsigned)?)?);
    bytes.push(b'\n');
    bytes.extend(tree);
    Ok(bytes)
}

impl Header {
    pub fn sign(&mut self, tree: &[u8], key: &SigningKey) -> Result<()> {
        self.sig = crate::keys::sign_bytes(&signed_bytes(self, tree)?, key);
        Ok(())
    }
    pub fn verify(&self, tree: &[u8], key: &VerifyingKey) -> Result<()> {
        if self.v != 2 {
            bail!("unsupported tree format {}", self.v);
        }
        if !is_uuid(&self.state) || !is_uuid(&self.machine) || !valid_attempt(&self.attempt) || self.parts > MAX_PARTS {
            bail!("invalid tree header");
        }
        if self.btrfs.as_ref().is_some_and(|i| !is_uuid(&i.uuid) || i.transid == 0) {
            bail!("invalid btrfs tree identity");
        }
        match (self.tree.as_str(), &self.base) {
            ("full", None) => {}
            ("delta", Some((state, attempt))) if is_uuid(state) && valid_attempt(attempt) => {}
            _ => bail!("invalid tree base"),
        }
        crate::keys::verify_bytes(&signed_bytes(self, tree)?, &self.sig, key)
    }
}

pub fn split_tree(object: &[u8]) -> Result<(Header, &[u8])> {
    let newline = object.iter().position(|&b| b == b'\n').context("tree has no header line")?;
    if newline > 16384 {
        bail!("tree header exceeds 16 KiB");
    }
    Ok((serde_json::from_slice(&object[..newline])?, &object[newline + 1..]))
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_tree_encoding_matches_store_canonical_bytes() {
        let state = "01a12290-0000-7000-8000-000000000003".to_string();
        let entry = Entry(
            "100644".into(),
            123456789,
            "ab".repeat(32),
            vec![
                Link(state.clone(), "0123abcd".into(), 0, 4096, 8192, "delta".into()),
                Link(state, "0123abcd".into(), 2, 0, 65536, "full".into()),
            ],
        );
        let tree = Tree {
            files: BTreeMap::from([("plain".into(), entry.clone()), ("caf\u{e9}/\u{1f642}".into(), entry.clone())]),
            times: BTreeMap::new(),
            chunks: BTreeMap::new(),
        };
        assert_eq!(tree.canonical().unwrap(), canonical(&serde_json::to_value(&tree).unwrap()).unwrap());
        Tree::parse(&tree.canonical().unwrap()).unwrap();
        let escaped = Tree { files: BTreeMap::from([("\n\t\\\"".into(), entry.clone())]), times: BTreeMap::new(), chunks: BTreeMap::new() };
        assert_eq!(escaped.canonical().unwrap(), canonical(&serde_json::to_value(&escaped).unwrap()).unwrap());
        assert_eq!(Tree::default().canonical().unwrap(), canonical(&serde_json::to_value(Tree::default()).unwrap()).unwrap());
        // File times: the same bytes as the generic canonical form, and a time needs its file.
        let timed = Tree { files: BTreeMap::from([("a".into(), entry.clone())]), times: BTreeMap::from([("a".into(), 1_793_820_425_267_530_733)]), chunks: BTreeMap::new() };
        assert_eq!(timed.canonical().unwrap(), canonical(&serde_json::to_value(&timed).unwrap()).unwrap());
        assert_eq!(Tree::parse(&timed.canonical().unwrap()).unwrap(), timed);
        // Chunks: canonical key order ("chunks" first), no links on a chunked file, valid ids.
        let id = "ab".repeat(32);
        let chunked_entry = Entry("100644".into(), 70_000, "cd".repeat(32), vec![]);
        let chunked = Tree {
            files: BTreeMap::from([("a".into(), entry.clone()), ("t/lib.rlib".into(), chunked_entry.clone())]),
            times: BTreeMap::from([("a".into(), 5)]),
            chunks: BTreeMap::from([("t/lib.rlib".into(), vec![id.clone(), id.clone()])]),
        };
        assert_eq!(chunked.canonical().unwrap(), canonical(&serde_json::to_value(&chunked).unwrap()).unwrap());
        assert_eq!(Tree::parse(&chunked.canonical().unwrap()).unwrap(), chunked);
        let mut both = chunked.clone();
        both.files.get_mut("t/lib.rlib").unwrap().3 = entry.3.clone();
        assert!(Tree::parse(&both.canonical().unwrap()).is_err(), "a chunked file with links");
        let mut neither = chunked.clone();
        neither.chunks.clear();
        assert!(Tree::parse(&neither.canonical().unwrap()).is_err(), "a file without links or chunks");
        for bad in [vec![], vec!["AB".repeat(32)], vec!["ab".repeat(31)], vec![id.clone(); 7]] {
            let mut tree = chunked.clone();
            tree.chunks.insert("t/lib.rlib".into(), bad);
            assert!(Tree::parse(&tree.canonical().unwrap()).is_err());
        }
        let mut missing = chunked.clone();
        missing.chunks.insert("gone".into(), vec![id]);
        assert!(Tree::parse(&missing.canonical().unwrap()).is_err(), "chunks of a missing file");
        let orphan = Tree { files: BTreeMap::from([("a".into(), entry)]), times: BTreeMap::from([("b".into(), 1)]), chunks: BTreeMap::new() };
        assert!(Tree::parse(&orphan.canonical().unwrap()).is_err());
    }
    use super::*;
    const ID: &str = "01a12290-0000-7000-8000-000000000001";
    #[test]
    fn signatures_bind_header_tree_and_domain() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let tree = b"{\"files\":{}}";
        let mut header = Header {
            v: 2,
            state: ID.into(),
            attempt: "0123abcd".into(),
            tree: "full".into(),
            base: None,
            parts: 0,
            machine: ID.into(),
            btrfs: None,
            sig: String::new(),
        };
        header.sign(tree, &key).unwrap();
        assert_eq!(header.sig, "yIMZ3tHimsJVUe2aJs7RJkVQr9mPaS8QazZnExdCmtVy2i063+jYI5TERQrvxlqYQsxOHTKk0yLlkvPfHfNiCQ==");
        header.verify(tree, &key.verifying_key()).unwrap();
        header.btrfs = Some(BtrfsIdentity { uuid: ID.into(), transid: 7 });
        header.sign(tree, &key).unwrap();
        assert_eq!(header.sig, "S7eww50UaRLUiyu646IdgZiC53cdiJQ7gTfMrT7cD9FnDeikWWfB3I4VC6LGDsUi7lSkm3ex6DkYBY7XCL+VDg==");
        header.verify(tree, &key.verifying_key()).unwrap();
        header.btrfs.as_mut().unwrap().transid = 8;
        assert!(header.verify(tree, &key.verifying_key()).is_err());
        header.btrfs.as_mut().unwrap().transid = 7;
        assert!(header.verify(b"{}", &key.verifying_key()).is_err());
        header.parts = 1;
        assert!(header.verify(tree, &key.verifying_key()).is_err());
    }
    #[test]
    fn object_names_are_strict() {
        assert_eq!(parse_tree_name(&tree_name(ID, "0123abcd")), Some((ID.into(), "0123abcd".into())));
        assert!(parse_tree_name("states/../0123abcd.tree.age").is_none());
        assert_eq!(parse_segment_name(&segment_name(ID, 1, 12)), Some((ID.into(), 1, 12)));
        for bad in ["../a", "/a", "a//b", "a/./b", "a\0"] {
            assert!(!safe_path(bad));
        }
    }
}

#[cfg(test)]
mod public_release_tests {
    use super::public_release_file;

    #[test]
    fn public_release_keys_follow_the_gateway_rules() {
        assert!(public_release_file("public/release/v0.33.0/layr-v0.33.0-linux-x86_64.tar.gz"));
        assert!(public_release_file("public/release/v1.2.3-rc.1/files.json"));
        assert!(!public_release_file("public/release/0.33.0/a"));
        assert!(!public_release_file("public/release/v1.2/a"));
        assert!(!public_release_file("public/release/v1.2.3/.hidden"));
        assert!(!public_release_file("public/release/v1.2.3/a/b"));
        assert!(!public_release_file("public/other/v1.2.3/a"));
    }
}
