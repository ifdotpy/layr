//! Content-defined chunks of cache files (FastCDC with normalized chunking).
//!
//! The cut points are part of the store format: the same bytes must give the same chunks in
//! every layr version, or chunks of an older push stop matching. So the gear table and the
//! masks below are fixed, and a test pins the cut points of a known input.

use hmac::{Hmac, Mac};
use sha2::Sha256;

/// No chunk is shorter, except the last chunk of a file.
pub const MIN: usize = 16 << 10;
/// The size that the masks aim at.
pub const AVG: usize = 64 << 10;
/// No chunk is longer.
pub const MAX: usize = 256 << 10;

/// Before `AVG` bytes a cut needs 18 zero bits (rarer), after it 14 (more likely). This keeps
/// most chunks near `AVG`. The bits are the high bits of the hash, which depend on the last 64
/// bytes; the low bits of a shifting gear hash depend on only a few bytes.
const MASK_SMALL: u64 = !0 << (64 - 18);
const MASK_LARGE: u64 = !0 << (64 - 14);

/// splitmix64 from seed 0: a fixed table that anyone can recompute.
const GEAR: [u64; 256] = {
    let mut table = [0u64; 256];
    let mut state = 0u64;
    let mut i = 0;
    while i < 256 {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        table[i] = z ^ (z >> 31);
        i += 1;
    }
    table
};

/// The length of the first chunk of `data`.
fn cut(data: &[u8]) -> usize {
    if data.len() <= MIN {
        return data.len();
    }
    let end = data.len().min(MAX);
    let normal = AVG.min(end);
    let mut hash = 0u64;
    let mut i = MIN;
    while i < normal {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_SMALL == 0 {
            return i + 1;
        }
        i += 1;
    }
    while i < end {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK_LARGE == 0 {
            return i + 1;
        }
        i += 1;
    }
    end
}

/// The chunks of `data` in order. An empty input has no chunks.
pub fn split(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(cut(rest));
        out.push(chunk);
        rest = tail;
    }
    out
}

/// Whether `id` has the form of a chunk id: 64 lowercase hex digits.
pub fn valid_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The name of a chunk: HMAC-SHA256 under the project's chunk key, in hex. The key keeps the
/// store from testing whether it holds a known chunk; equal chunks still get equal names.
pub fn id(key: &[u8; 32], chunk: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    mac.update(chunk);
    hex::encode(mac.finalize().into_bytes())
}

/// The chunk key of a cache layer epoch: HKDF-SHA256 (RFC 5869) with the layer identity as the
/// input key, the epoch id as the salt and a fixed label. Only holders of the layer identity can
/// name chunks, and a new epoch gets new names.
pub fn key(identity: &str, epoch: &str) -> [u8; 32] {
    let mut extract = Hmac::<Sha256>::new_from_slice(epoch.as_bytes()).expect("any key length");
    extract.update(identity.as_bytes());
    let prk = extract.finalize().into_bytes();
    let mut expand = Hmac::<Sha256>::new_from_slice(&prk).expect("any key length");
    expand.update(b"layr cache chunk key v1");
    expand.update(&[1]);
    expand.finalize().into_bytes().into()
}

/// A pack aims at this much plaintext; the last chunk may pass it.
pub const PACK_TARGET: usize = 4 << 20;
/// Packs are larger only by one chunk; a reader refuses more.
pub const PACK_LIMIT: usize = PACK_TARGET + MAX;
/// The zstd level of a pack: packs are written once and read many times.
pub const PACK_LEVEL: i32 = 12;

/// The names of a pack and of its index, inside the layer prefix.
pub fn pack_name(pack: &str) -> String {
    format!("chunks/{pack}.pack.age")
}
pub fn index_name(pack: &str) -> String {
    format!("chunks/{pack}.index.age")
}
/// The pack id of a `chunks/` object, and whether it is the index.
pub fn parse_name(key: &str) -> Option<(String, bool)> {
    let rest = key.strip_prefix("chunks/")?;
    let (pack, index) = match (rest.strip_suffix(".pack.age"), rest.strip_suffix(".index.age")) {
        (Some(pack), None) => (pack, false),
        (None, Some(pack)) => (pack, true),
        _ => return None,
    };
    crate::format::is_uuid(pack).then(|| (pack.to_string(), index))
}

/// Where each chunk of a pack is in the pack's plaintext. The index is a separate object, so a
/// writer can learn which chunks a store has, and a deletion can find live and dead bytes,
/// without reading pack data.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub v: u32,
    /// `[id, offset, length]` in plaintext order, without gaps.
    pub chunks: Vec<(String, u64, u64)>,
}

impl Index {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = String::from("{\"chunks\":[");
        for (number, (id, offset, length)) in self.chunks.iter().enumerate() {
            if number != 0 {
                out.push(',');
            }
            out.push_str(&format!("[\"{id}\",{offset},{length}]"));
        }
        out.push_str(&format!("],\"v\":{}}}", self.v));
        out.into_bytes()
    }
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let index: Self = serde_json::from_slice(bytes)?;
        anyhow::ensure!(index.v == 1, "unsupported chunk index version {}", index.v);
        anyhow::ensure!(index.encode() == bytes, "noncanonical chunk index");
        anyhow::ensure!(!index.chunks.is_empty(), "empty chunk index");
        let mut end = 0;
        for (id, offset, length) in &index.chunks {
            anyhow::ensure!(valid_id(id), "invalid chunk id");
            anyhow::ensure!(*offset == end && *length > 0 && *length <= MAX as u64, "invalid chunk range");
            end += length;
        }
        anyhow::ensure!(end <= PACK_LIMIT as u64, "pack exceeds its size limit");
        Ok(index)
    }
    /// The plaintext size of the pack.
    pub fn size(&self) -> u64 {
        self.chunks.last().map_or(0, |(_, offset, length)| offset + length)
    }
}

/// A pack: chunks in order, as one zstd frame. `add` until `full`, then `finish`.
#[cfg(feature = "native")]
#[derive(Default)]
pub struct Builder {
    data: Vec<u8>,
    index: Index,
}

#[cfg(feature = "native")]
impl Builder {
    pub fn add(&mut self, id: String, chunk: &[u8]) {
        self.index.chunks.push((id, self.data.len() as u64, chunk.len() as u64));
        self.data.extend_from_slice(chunk);
    }
    pub fn is_empty(&self) -> bool {
        self.index.chunks.is_empty()
    }
    pub fn full(&self) -> bool {
        self.data.len() >= PACK_TARGET
    }
    /// The compressed pack and its index.
    pub fn finish(self) -> anyhow::Result<(Vec<u8>, Index)> {
        let frame = crate::zst::compress_level(&self.data, PACK_LEVEL)?;
        Ok((frame, Index { v: 1, ..self.index }))
    }
}

/// The plaintext of a pack, checked against its index.
pub fn open_pack(frame: &[u8], index: &Index) -> anyhow::Result<Vec<u8>> {
    let data = crate::zst::decompress_with_limit(frame, None, PACK_LIMIT)?;
    anyhow::ensure!(data.len() as u64 == index.size(), "pack size differs from its index");
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 56) as u8
            })
            .collect()
    }

    #[test]
    fn chunks_cover_the_input_within_bounds() {
        for len in [0, 1, MIN - 1, MIN, MIN + 1, AVG, MAX, MAX + 1, 3 * MAX + 17, 4 << 20] {
            let data = noise(len, len as u64);
            let chunks = split(&data);
            assert_eq!(chunks.concat(), data);
            for (i, chunk) in chunks.iter().enumerate() {
                assert!(chunk.len() <= MAX);
                assert!(chunk.len() >= MIN || i + 1 == chunks.len(), "short chunk inside the input");
            }
        }
    }

    #[test]
    fn cut_points_are_fixed() {
        // A change here changes the store format: chunks of older pushes would stop matching.
        let lengths: Vec<usize> = split(&noise(1 << 20, 7)).iter().map(|c| c.len()).collect();
        assert_eq!(GEAR[0], 0xe220_a839_7b1d_cdaf);
        assert_eq!(lengths.iter().sum::<usize>(), 1 << 20);
        assert_eq!(lengths, PINNED.to_vec());
    }
    const PINNED: &[usize] = &[83931, 66477, 104326, 58501, 72892, 66658, 66748, 81136, 67277, 68591, 82002, 117972, 99038, 13027];

    #[test]
    fn an_insertion_changes_only_nearby_chunks() {
        let data = noise(2 << 20, 11);
        let mut changed = data.clone();
        changed.splice(1 << 20..1 << 20, b"inserted bytes".iter().copied());
        let before: std::collections::BTreeSet<&[u8]> = split(&data).into_iter().collect();
        let after = split(&changed);
        let new = after.iter().filter(|c| !before.contains(*c)).count();
        assert!(new <= 2, "{new} of {} chunks changed", after.len());
    }

    #[test]
    fn keys_depend_on_identity_and_epoch() {
        let a = key("AGE-SECRET-KEY-1ONE", "epoch-1");
        assert_eq!(a, key("AGE-SECRET-KEY-1ONE", "epoch-1"));
        assert_ne!(a, key("AGE-SECRET-KEY-1TWO", "epoch-1"));
        assert_ne!(a, key("AGE-SECRET-KEY-1ONE", "epoch-2"));
        // RFC 5869 test case 1 checks the construction itself.
        let mut extract = Hmac::<Sha256>::new_from_slice(&hex::decode("000102030405060708090a0b0c").unwrap()).unwrap();
        extract.update(&[0x0b; 22]);
        let prk = extract.finalize().into_bytes();
        assert_eq!(hex::encode(prk), "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5");
    }

    #[test]
    fn pack_names_and_indexes_are_strict() {
        let pack = "01a12290-0000-7000-8000-000000000001";
        assert_eq!(parse_name(&pack_name(pack)), Some((pack.into(), false)));
        assert_eq!(parse_name(&index_name(pack)), Some((pack.into(), true)));
        assert_eq!(parse_name("chunks/../x.pack.age"), None);
        let index = Index { v: 1, chunks: vec![("ab".repeat(32), 0, 10), ("cd".repeat(32), 10, 5)] };
        assert_eq!(Index::parse(&index.encode()).unwrap(), index);
        assert_eq!(index.size(), 15);
        for bad in [
            Index { v: 2, ..index.clone() },
            Index { v: 1, chunks: vec![] },
            Index { v: 1, chunks: vec![("ab".repeat(32), 1, 10)] },
            Index { v: 1, chunks: vec![("ab".repeat(32), 0, 0)] },
            Index { v: 1, chunks: vec![("AB".repeat(32), 0, 10)] },
        ] {
            assert!(Index::parse(&bad.encode()).is_err());
        }
    }

    #[cfg(feature = "native")]
    #[test]
    fn a_pack_round_trips_and_checks_its_size() {
        let mut builder = Builder::default();
        let one = noise(70_000, 1);
        let two = noise(30_000, 2);
        builder.add(id(&[1; 32], &one), &one);
        builder.add(id(&[1; 32], &two), &two);
        assert!(!builder.full());
        let (frame, index) = builder.finish().unwrap();
        let data = open_pack(&frame, &index).unwrap();
        let (_, offset, length) = &index.chunks[1];
        assert_eq!(&data[*offset as usize..(*offset + *length) as usize], two.as_slice());
        let short = Index { v: 1, chunks: index.chunks[..1].to_vec() };
        assert!(open_pack(&frame, &short).is_err());
    }

    #[test]
    fn ids_depend_on_the_key_and_the_content() {
        let a = id(&[1; 32], b"chunk");
        assert_eq!(a.len(), 64);
        assert_eq!(a, id(&[1; 32], b"chunk"));
        assert_ne!(a, id(&[2; 32], b"chunk"));
        assert_ne!(a, id(&[1; 32], b"chunk!"));
    }
}
