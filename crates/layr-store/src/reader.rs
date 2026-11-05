//! V2 readers use only object bytes supplied by the caller.

use crate::{format, keys, zst};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_hashers::{ContentAddress, Digest};
use std::collections::BTreeMap;

/// Decrypt one tree object and verify its header and canonical tree.
/// For a delta, supply the verified canonical bytes of the base tree.
pub fn tree(
    identity: &str,
    object: &[u8],
    base: Option<&[u8]>,
    key: &VerifyingKey,
) -> Result<(format::Header, Vec<u8>, format::Tree)> {
    let plain = keys::decrypt(identity, object)?;
    let (header, frame) = format::split_tree(&plain)?;
    let base = match header.tree.as_str() {
        "full" => None,
        "delta" => Some(base.context("delta tree has no base bytes")?),
        _ => bail!("invalid tree kind"),
    };
    let canonical = zst::decompress(frame, base)?;
    header.verify(&canonical, key)?;
    let tree = format::Tree::parse(&canonical)?;
    Ok((header, canonical, tree))
}

/// Read a file from encrypted data parts supplied by their object names.
/// The entry must come from a verified tree.
pub fn file(identity: &str, entry: &format::Entry, parts: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
    if entry.3.is_empty() {
        bail!("a chunked file of a cache layer: read it with reader::chunked_file");
    }
    if entry.3.len() > format::MAX_FILE_DELTAS + 1 {
        bail!("invalid file chain length");
    }
    let mut data = Vec::new();
    for (index, link) in entry.3.iter().enumerate().rev() {
        let expected = if index + 1 == entry.3.len() { "full" } else { "delta" };
        if link.5 != expected {
            bail!("file chain must end with one full frame");
        }
        let name = link.object();
        let object = parts.get(&name).with_context(|| format!("missing data part {name}"))?;
        let plain = keys::decrypt(identity, object)?;
        let start = usize::try_from(link.3)?;
        let end = usize::try_from(link.3.checked_add(link.4).context("frame range overflow")?)?;
        let frame = plain.get(start..end).context("frame is outside its part")?;
        let limit = if index == 0 { usize::try_from(entry.1)? } else { zst::LIMIT };
        data = zst::decompress_with_limit(frame, if link.5 == "delta" { Some(&data) } else { None }, limit)?;
    }
    if data.len() as u64 != entry.1 || hex::encode(ContentAddress::digest(&data)) != entry.2 {
        bail!("file size or SHA-256 does not match its tree");
    }
    Ok(data)
}

/// Read a chunked file of a cache layer (`format::Tree::chunks`). `packs` holds the encrypted pack
/// objects and `indexes` the verified indexes, both by pack id; the caller finds them by listing
/// `chunks/` of the layer. The entry and the ids must come from a verified tree.
pub fn chunked_file(
    identity: &str,
    entry: &format::Entry,
    ids: &[String],
    indexes: &BTreeMap<String, crate::chunks::Index>,
    packs: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut places = BTreeMap::new();
    for (pack, index) in indexes {
        for (id, offset, length) in &index.chunks {
            places.entry(id.as_str()).or_insert((pack.as_str(), *offset, *length));
        }
    }
    let mut opened: BTreeMap<&str, Vec<u8>> = BTreeMap::new();
    let mut data = Vec::new();
    for id in ids {
        let &(pack, offset, length) = places.get(id.as_str()).with_context(|| format!("chunk {id} has no pack"))?;
        if !opened.contains_key(pack) {
            let object = packs.get(pack).with_context(|| format!("missing chunk pack {pack}"))?;
            opened.insert(pack, crate::chunks::open_pack(&keys::decrypt(identity, object)?, &indexes[pack])?);
        }
        let chunk = opened[pack].get(offset as usize..(offset + length) as usize).context("chunk is outside its pack")?;
        if data.len() as u64 + length > entry.1 {
            bail!("chunks exceed their file size");
        }
        data.extend_from_slice(chunk);
    }
    if data.len() as u64 != entry.1 || hex::encode(ContentAddress::digest(&data)) != entry.2 {
        bail!("file size or SHA-256 does not match its tree");
    }
    Ok(data)
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn supplied_objects_restore_and_authenticate_a_file() {
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let state = "01a12290-0000-7000-8000-000000000001";
        let attempt = "0123abcd";
        let data = b"file bytes";
        let frame = zst::compress(data, None).unwrap();
        let link = format::Link(state.into(), attempt.into(), 0, 0, frame.len() as u64, "full".into());
        let mut parts = BTreeMap::new();
        parts.insert(link.object(), keys::encrypt(&recipient, &frame).unwrap());
        let entry = format::Entry("100644".into(), data.len() as u64, hex::encode(ContentAddress::digest(data)), vec![link]);
        let source = format::Tree { files: BTreeMap::from([("file".into(), entry)]), times: BTreeMap::new(), chunks: BTreeMap::new() };
        let canonical = source.canonical().unwrap();
        let mut header = format::Header {
            v: 2,
            state: state.into(),
            attempt: attempt.into(),
            tree: "full".into(),
            base: None,
            parts: 1,
            machine: state.into(),
            btrfs: None,
            sig: String::new(),
        };
        header.sign(&canonical, &key).unwrap();
        let mut plain = serde_json::to_vec(&header).unwrap();
        plain.push(b'\n');
        plain.extend(zst::compress(&canonical, None).unwrap());
        let encrypted = keys::encrypt(&recipient, &plain).unwrap();
        let (_, decoded, parsed) = tree(&identity, &encrypted, None, &key.verifying_key()).unwrap();
        assert_eq!(decoded, canonical);
        assert_eq!(file(&identity, &parsed.files["file"], &parts).unwrap(), data);
        assert!(tree(&identity, &encrypted, None, &SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err());
        let mut wrong = parsed.files["file"].clone();
        wrong.2 = "0".repeat(64);
        assert!(file(&identity, &wrong, &parts).is_err());
        assert!(file(&identity, &parsed.files["file"], &BTreeMap::new()).is_err());
    }

    #[test]
    fn supplied_packs_restore_and_authenticate_a_chunked_file() {
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let key = crate::chunks::key(&identity, "epoch");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let mut builder = crate::chunks::Builder::default();
        let mut ids = Vec::new();
        for chunk in crate::chunks::split(&data) {
            let id = crate::chunks::id(&key, chunk);
            builder.add(id.clone(), chunk);
            ids.push(id);
        }
        let (frame, index) = builder.finish().unwrap();
        let pack = "01a12290-0000-7000-8000-000000000009".to_string();
        let indexes = BTreeMap::from([(pack.clone(), index)]);
        let packs = BTreeMap::from([(pack, keys::encrypt(&recipient, &frame).unwrap())]);
        let entry = format::Entry("100644".into(), data.len() as u64, hex::encode(ContentAddress::digest(&data)), vec![]);
        assert_eq!(chunked_file(&identity, &entry, &ids, &indexes, &packs).unwrap(), data);
        assert!(file(&identity, &entry, &BTreeMap::new()).is_err(), "frames reader refuses a chunked file");
        let mut wrong = entry.clone();
        wrong.2 = "0".repeat(64);
        assert!(chunked_file(&identity, &wrong, &ids, &indexes, &packs).is_err());
        assert!(chunked_file(&identity, &entry, &ids[1..], &indexes, &packs).is_err(), "a missing chunk changes the hash");
        assert!(chunked_file(&identity, &entry, &ids, &indexes, &BTreeMap::new()).is_err());
    }
}
